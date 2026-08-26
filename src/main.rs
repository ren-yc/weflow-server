use anyhow::Result;

fn main() -> Result<()> {
    let cfg = match weflow_server::config::parse_args()? {
        Some(c) => c,
        None => return Ok(()), // --help / --version already printed
    };
    weflow_server::logging::init(&cfg.log);
    weflow_server::run(cfg)
}