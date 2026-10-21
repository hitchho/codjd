//! The shared shape of a patch between the document layer (files, actions, import) and the
//! engine (compiler, real-time program): a validated graph with ids resolved to indices.
//! See docs/patches.md.

use std::collections::BTreeMap;

/// Every node type of docs/patches.md, with its inputs, outputs and their defaults
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum NodeKind {
    Const,
    Knob,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Equal,
    Greater,
    Nop,
    Sine,
    Saw,
    Tri,
    Pulse,
    Noise,
    Adsr,
    Filter,
    Delay,
    Distort,
    Slide,
    Hold,
    Clock,
    ClockDiv,
    MonoSeq,
    GateSeq,
    Kick,
    Snare,
    Clap,
    HatClosed,
    HatOpen,
    Rim,
    Tom,
    Pan,
    Mix4,
    AudioOut,
    Notes,
}

/// Inputs (name, default), outputs and the type name used in files
pub struct Schema {
    pub name: &'static str,
    pub inputs: &'static [(&'static str, f32)],
    pub outputs: &'static [&'static str],
}

const DRUM_INS: &[(&str, f32)] = &[("trig", 0.), ("level", 1.), ("tune", 0.), ("decay", 0.)];
const MATH_INS: &[(&str, f32)] = &[("in0", 0.), ("in1", 0.)];
const OUT: &[&str] = &["out"];

impl NodeKind {
    pub const ALL: &'static [NodeKind] = &[
        NodeKind::Const,
        NodeKind::Knob,
        NodeKind::Add,
        NodeKind::Sub,
        NodeKind::Mul,
        NodeKind::Div,
        NodeKind::Mod,
        NodeKind::Equal,
        NodeKind::Greater,
        NodeKind::Nop,
        NodeKind::Sine,
        NodeKind::Saw,
        NodeKind::Tri,
        NodeKind::Pulse,
        NodeKind::Noise,
        NodeKind::Adsr,
        NodeKind::Filter,
        NodeKind::Delay,
        NodeKind::Distort,
        NodeKind::Slide,
        NodeKind::Hold,
        NodeKind::Clock,
        NodeKind::ClockDiv,
        NodeKind::MonoSeq,
        NodeKind::GateSeq,
        NodeKind::Kick,
        NodeKind::Snare,
        NodeKind::Clap,
        NodeKind::HatClosed,
        NodeKind::HatOpen,
        NodeKind::Rim,
        NodeKind::Tom,
        NodeKind::Pan,
        NodeKind::Mix4,
        NodeKind::AudioOut,
        NodeKind::Notes,
    ];

    pub fn schema(self) -> Schema {
        use NodeKind::*;
        let s = |name, inputs, outputs| Schema { name, inputs, outputs };
        match self {
            Const => s("Const", &[], OUT),
            Knob => s("Knob", &[], OUT),
            Add => s("Add", MATH_INS, OUT),
            Sub => s("Sub", MATH_INS, OUT),
            Mul => s("Mul", MATH_INS, OUT),
            Div => s("Div", MATH_INS, OUT),
            Mod => s("Mod", MATH_INS, OUT),
            Equal => s("Equal", MATH_INS, OUT),
            Greater => s("Greater", MATH_INS, OUT),
            Nop => s("Nop", &[("in", 0.)], OUT),
            Sine => s("Sine", &[("freq", 440.), ("sync", 0.)], OUT),
            Saw => s("Saw", &[("freq", 440.)], OUT),
            Tri => s("Tri", &[("freq", 440.)], OUT),
            Pulse => s("Pulse", &[("freq", 440.), ("pw", 0.5)], OUT),
            Noise => s("Noise", &[], OUT),
            Adsr => s("ADSR", &[("gate", 0.), ("att", 0.02), ("dec", 0.1), ("sus", 0.2), ("rel", 0.1)], OUT),
            Filter => s("Filter", &[("in", 0.), ("cutoff", 1.), ("reso", 0.)], OUT),
            Delay => s("Delay", &[("in", 0.), ("time", 0.25)], OUT),
            Distort => s("Distort", &[("in", 0.), ("amt", 0.)], OUT),
            Slide => s("Slide", &[("in", 0.), ("rate", 1.)], OUT),
            Hold => s("Hold", &[("in", 0.), ("trig", 0.)], OUT),
            Clock => s("Clock", &[], &["out", "beat", "bar"]),
            ClockDiv => s("ClockDiv", &[("clock", 0.)], OUT),
            MonoSeq => s("MonoSeq", &[("clock", 0.), ("gateT", 0.5)], &["freq", "gate"]),
            GateSeq => s(
                "GateSeq",
                &[("clock", 0.), ("gateT", 0.5)],
                &["gate0", "gate1", "gate2", "gate3", "gate4", "gate5", "gate6", "gate7"],
            ),
            Kick => s("Kick", DRUM_INS, OUT),
            Snare => s("Snare", DRUM_INS, OUT),
            Clap => s("Clap", DRUM_INS, OUT),
            HatClosed => s("HatClosed", DRUM_INS, OUT),
            HatOpen => s("HatOpen", DRUM_INS, OUT),
            Rim => s("Rim", DRUM_INS, OUT),
            Tom => s("Tom", DRUM_INS, OUT),
            Pan => s("Pan", &[("in", 0.), ("pos", 0.)], &["left", "right"]),
            Mix4 => s(
                "Mix4",
                &[
                    ("in0", 0.),
                    ("in1", 0.),
                    ("in2", 0.),
                    ("in3", 0.),
                    ("g0", 1.),
                    ("g1", 1.),
                    ("g2", 1.),
                    ("g3", 1.),
                ],
                OUT,
            ),
            AudioOut => s("AudioOut", &[("left", 0.), ("right", 0.)], &[]),
            Notes => s("Notes", &[], &[]),
        }
    }

    /// The type of a file's `"type"` (case-sensitive, as in docs/patches.md)
    pub fn from_name(name: &str) -> Option<NodeKind> {
        NodeKind::ALL.iter().copied().find(|k| k.schema().name == name)
    }

    pub fn input_index(self, input: &str) -> Option<usize> {
        self.schema().inputs.iter().position(|(n, _)| *n == input)
    }

    pub fn output_index(self, output: &str) -> Option<usize> {
        self.schema().outputs.iter().position(|n| *n == output)
    }
}

/// One step of a sequencer pattern
#[derive(Clone, Debug, PartialEq)]
pub struct Cell {
    pub step: u16,
    pub row: u8,
    /// 0..1, 1 when the file doesn't say
    pub velocity: f32,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Pattern {
    pub steps: u16,
    pub cells: Vec<Cell>,
}

/// A node parameter as written in the file
#[derive(Clone, Debug, PartialEq)]
pub enum ParamValue {
    Num(f64),
    Text(String),
    Patterns(Vec<Pattern>),
}

pub type Params = BTreeMap<String, ParamValue>;

/// Where an input's signal comes from
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Input {
    /// the node type's default
    Default,
    Const(f32),
    From { node: usize, output: usize },
}

#[derive(Clone, Debug, PartialEq)]
pub struct NodeSpec {
    /// the file's node id, kept so live edits and state carry-over can match nodes
    pub id: String,
    pub kind: NodeKind,
    pub params: Params,
    /// in the order of the node type's inputs (`kind.schema().inputs`)
    pub inputs: Vec<Input>,
}

/// A validated patch: ids resolved to indices, modules flattened, exactly one `AudioOut`
#[derive(Clone, Debug, PartialEq, Default)]
pub struct PatchGraph {
    pub nodes: Vec<NodeSpec>,
    /// knob name -> node index, in file order
    pub knobs: Vec<(String, usize)>,
    /// sequencer name -> node index (`MonoSeq`/`GateSeq`)
    pub sequencers: Vec<(String, usize)>,
}

impl NodeSpec {
    pub fn num(&self, param: &str) -> Option<f64> {
        match self.params.get(param) {
            Some(ParamValue::Num(v)) => Some(*v),
            _ => None,
        }
    }

    pub fn text(&self, param: &str) -> Option<&str> {
        match self.params.get(param) {
            Some(ParamValue::Text(t)) => Some(t),
            _ => None,
        }
    }

    pub fn patterns(&self) -> &[Pattern] {
        match self.params.get("patterns") {
            Some(ParamValue::Patterns(p)) => p,
            _ => &[],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_and_schemas_are_consistent() {
        for &k in NodeKind::ALL {
            let s = k.schema();
            assert_eq!(NodeKind::from_name(s.name), Some(k), "{}", s.name);
            let mut ins: Vec<_> = s.inputs.iter().map(|(n, _)| *n).collect();
            ins.dedup();
            assert_eq!(ins.len(), s.inputs.len(), "duplicate input on {}", s.name);
        }
        assert_eq!(NodeKind::Filter.input_index("cutoff"), Some(1));
        assert_eq!(NodeKind::MonoSeq.output_index("gate"), Some(1));
        assert_eq!(NodeKind::from_name("filter"), None, "case-sensitive like the files");
    }
}
