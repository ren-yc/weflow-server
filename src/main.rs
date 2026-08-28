use anyhow::Result;

fn main() {
    if let Err(e) = run() {
        eprintln!("[fatal] {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let Some(cfg) = weflow_server::config::load()? else {
        return Ok(()); // --help / --version already printed
    };
    weflow_server::logging::init(&cfg.log);
    weflow_server::run(cfg)
}
