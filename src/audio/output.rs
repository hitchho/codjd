//! Output devices. The main device's callback runs the mixer; a second device (headphones in
//! internal mode, deck B in external mode) is fed from it through a ring buffer.
//!
//! The two devices have separate clocks. The ring absorbs the jitter between their callbacks;
//! if it runs dry the second output plays silence, if it overfills (the main clock is faster) the
//! second output skips ahead. Both are counted in [`Shared`], to see how much drift a pair of
//! devices really has before adding a resampler.

use std::sync::{atomic::Ordering, Arc};

use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Device, Stream, StreamConfig,
};
use tracing::{info, warn};

use super::mixer::{Buses, Mixer, Mode, Shared};

/// Frames the second output keeps buffered before it starts, and skips back to when overfull
const SECOND_TARGET: usize = 2048;
const SECOND_CAPACITY: usize = 16384;

pub struct Outputs {
    _streams: Vec<Stream>,
    pub rate: u32,
}

/// Finds an output device: "default", or part of its name (case-insensitive)
pub fn find_device(name: &str) -> Result<Device, String> {
    let host = cpal::default_host();
    if name.is_empty() || name == "default" {
        return host
            .default_output_device()
            .ok_or_else(|| "no default output device".to_string());
    }
    let wanted = name.to_lowercase();
    let devices = host.output_devices().map_err(|e| e.to_string())?;
    let mut names = Vec::new();
    for device in devices {
        let label = device_name(&device);
        if label.to_lowercase().contains(&wanted) {
            return Ok(device);
        }
        names.push(label);
    }
    Err(format!(
        "no output device matching '{name}'; available: {}",
        names.join(", ")
    ))
}

pub fn device_name(device: &Device) -> String {
    device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| device.to_string())
}

/// Names of all output devices
pub fn list_devices() -> Vec<String> {
    cpal::default_host()
        .output_devices()
        .map(|devices| devices.map(|d| device_name(&d)).collect())
        .unwrap_or_default()
}

/// Which bus a device plays: (left, right) of a frame
fn main_bus(mode: Mode, b: &Buses) -> (f32, f32) {
    match mode {
        Mode::Internal => b.master,
        Mode::External => b.decks[0],
    }
}

fn second_bus(mode: Mode, b: &Buses) -> (f32, f32) {
    match mode {
        Mode::Internal => b.cue,
        Mode::External => b.decks[1],
    }
}

/// Opens the outputs and starts the mixer in the main device's callback
pub fn start(
    main: &Device,
    second: Option<&Device>,
    mixer_for_rate: impl FnOnce(u32) -> Mixer,
    shared: Arc<Shared>,
) -> Result<Outputs, String> {
    let default = main.default_output_config().map_err(|e| e.to_string())?;
    let rate = default.sample_rate();
    let config = StreamConfig {
        channels: default.channels().max(2),
        sample_rate: rate,
        buffer_size: cpal::BufferSize::Default,
    };
    let main_channels = config.channels as usize;
    let mut mixer = mixer_for_rate(rate);

    let (mut to_second, from_main) = rtrb::RingBuffer::<(f32, f32)>::new(SECOND_CAPACITY);
    let has_second = second.is_some();

    let errors = Arc::clone(&shared);
    let main_stream = main
        .build_output_stream::<f32, _, _>(
            config,
            move |data, _| {
                mixer.take_commands();
                let mode = mixer.mode();
                for frame in data.chunks_exact_mut(main_channels) {
                    let buses = mixer.next();
                    let (l, r) = main_bus(mode, &buses);
                    frame[0] = l;
                    frame[1] = r;
                    for extra in &mut frame[2..] {
                        *extra = 0.;
                    }
                    if has_second {
                        // a full ring means the second output stalled; drop rather than wait
                        let _ = to_second.push(second_bus(mode, &buses));
                    }
                }
                mixer.publish();
            },
            move |e| {
                errors.xruns.fetch_add(1, Ordering::Relaxed);
                warn!(error = %e, "main output");
            },
            None,
        )
        .map_err(|e| e.to_string())?;
    let mut streams = vec![main_stream];

    if let Some(device) = second {
        let channels = device
            .default_output_config()
            .map(|c| c.channels().max(2))
            .unwrap_or(2);
        let config = StreamConfig {
            channels,
            // ask for the main rate; PipeWire converts if the device runs at another one
            sample_rate: rate,
            buffer_size: cpal::BufferSize::Default,
        };
        let channels = channels as usize;
        let mut from_main = from_main;
        let mut primed = false;
        let counters = Arc::clone(&shared);
        let errors = Arc::clone(&shared);
        let stream = device
            .build_output_stream::<f32, _, _>(
                config,
                move |data, _| {
                    let available = from_main.slots();
                    if !primed && available >= SECOND_TARGET {
                        primed = true;
                    }
                    if primed && available > SECOND_CAPACITY - SECOND_TARGET {
                        // the main clock runs faster: drop the surplus
                        let skip = available - SECOND_TARGET;
                        if let Ok(chunk) = from_main.read_chunk(skip) {
                            chunk.commit_all();
                        }
                        counters.second_skips.fetch_add(1, Ordering::Relaxed);
                    }
                    let mut dry = false;
                    for frame in data.chunks_exact_mut(channels) {
                        let (l, r) = if primed {
                            from_main.pop().unwrap_or_else(|_| {
                                dry = true;
                                (0., 0.)
                            })
                        } else {
                            (0., 0.)
                        };
                        frame[0] = l;
                        frame[1] = r;
                        for extra in &mut frame[2..] {
                            *extra = 0.;
                        }
                    }
                    if dry {
                        counters.second_underruns.fetch_add(1, Ordering::Relaxed);
                        // build the cushion up again
                        primed = false;
                    }
                },
                move |e| {
                    errors.xruns.fetch_add(1, Ordering::Relaxed);
                    warn!(error = %e, "second output");
                },
                None,
            )
            .map_err(|e| e.to_string())?;
        streams.push(stream);
    }

    for stream in &streams {
        stream.play().map_err(|e| e.to_string())?;
    }
    info!(
        main = %device_name(main),
        second = %second.map(device_name).unwrap_or_default(),
        rate,
        "audio started"
    );
    Ok(Outputs {
        _streams: streams,
        rate,
    })
}
