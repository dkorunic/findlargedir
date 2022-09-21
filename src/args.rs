use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
pub struct Args {
    #[clap(short = 'a', long, action = clap::ArgAction::Set, default_value_t = false)]
    pub accurate: bool,

    #[clap(short = 'o', long, action = clap::ArgAction::Set, default_value_t = true)]
    pub one_filesystem: bool,

    #[clap(short = 'c', long, value_parser, default_value_t = crate::calibrate::DEFAULT_TEST_COUNT)]
    pub calibration_count: u64,

    #[clap(short = 't', long, value_parser)]
    pub calibration_path: Option<PathBuf>,

    #[clap(short = 'A', long, value_parser, default_value_t = crate::walk::ALERT_COUNT)]
    pub alert_threshold: u64,

    #[clap(short = 'B', long, value_parser, default_value_t = crate::walk::BLACKLIST_COUNT)]
    pub blacklist_threshold: u64,

    #[clap(required = true, value_parser)]
    pub path: Vec<PathBuf>,
}
