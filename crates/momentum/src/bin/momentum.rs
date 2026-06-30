use clap::Parser;

#[cfg(target_os = "macos")]
#[used]
#[unsafe(link_section = "__TEXT,__info_plist")]
static INFO_PLIST: [u8; include_bytes!("../macos/Info.plist").len()] =
    *include_bytes!("../macos/Info.plist");

fn main() -> anyhow::Result<()> {
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    momentum::ensure_macos_responsibility()?;

    let cli = momentum::Cli::parse();
    momentum::init_logging(cli.debug);
    momentum::run_main(cli)
}
