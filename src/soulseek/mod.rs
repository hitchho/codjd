mod client;
#[allow(dead_code)]
mod download;
mod listener;
#[allow(dead_code)]
mod messages;
mod peer;
#[allow(dead_code)]
mod protocol;
#[allow(dead_code)]
mod types;

pub use client::SoulseekClient;
pub use download::{find_alternates, rank_search_results};
pub use types::{
    ConnectionState, Download, DownloadState, RankedFile, SearchPrefs, SearchResult,
    SoulseekConfig,
};
