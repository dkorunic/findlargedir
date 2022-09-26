use clap::Parser;
use clap::ValueHint;
use std::path::PathBuf;

#[derive(Parser, Default, Debug)]
#[clap(author, version, about, long_about = None)]
pub struct Args {
    // TODO: Perform accurate directory entry counting
    #[clap(short = 'a', long, action = clap::ArgAction::Set, default_value_t = false)]
    pub accurate: bool,

    // Do not cross mount points
    #[clap(short = 'o', long, action = clap::ArgAction::Set, default_value_t = true)]
    pub one_filesystem: bool,

    // Calibration entry count
    #[clap(short = 'c', long, value_parser, default_value_t = crate::calibrate::DEFAULT_TEST_COUNT)]
    pub calibration_count: u64,

    // Calibration path
    #[clap(short = 't', long, value_parser, value_hint = ValueHint::AnyPath)]
    pub calibration_path: Option<PathBuf>,

    // Alert threshold count: just print the estimate
    #[clap(short = 'A', long, value_parser, default_value_t = crate::walk::ALERT_COUNT)]
    pub alert_threshold: u64,

    // Blacklist threshold count: print the estimate and stop further deeper scan
    #[clap(short = 'B', long, value_parser, default_value_t = crate::walk::BLACKLIST_COUNT)]
    pub blacklist_threshold: u64,

    // Directories to never scan
    #[clap(short = 's', long, value_parser, value_hint = ValueHint::AnyPath)]
    pub skip_path: Vec<PathBuf>,

    // Paths to perform deep scan on
    #[clap(required = true, value_parser, value_hint = ValueHint::AnyPath)]
    pub path: Vec<PathBuf>,
}
