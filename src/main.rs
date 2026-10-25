//! The `devmounts` binary. Everything it does is [`devmounts::cli`]; this hands it the process —
//! first to the privileged helper, when this run is one ([`devmounts::privileged::try_serve`]).

use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    if devmounts::privileged::try_serve() {
        return ExitCode::SUCCESS;
    }
    ExitCode::from(devmounts::cli::Cli::parse().run())
}
