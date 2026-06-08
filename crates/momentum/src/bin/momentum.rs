use clap::Parser;

fn main() -> anyhow::Result<()> {
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let cli = momentum::Cli::parse();
    momentum::init_logging(cli.debug);
    momentum::run(cli)
}
