//! Developer CLI for the Linux host: `probe`, `encode-test`, `serve`.

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    secondscrn_linux_host::cli::main()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("secondscrn-linux-cli runs on Linux only");
}
