use std::fmt;
use std::sync::Arc;
use std::net::{TcpStream, ToSocketAddrs};
use std::io::prelude::*;
use std::process::{exit, Command};
use std::path::PathBuf;

use my_daemonize;

#[macro_use]
mod toml;
mod cmd;
mod proxy_config;
mod running_config;

use self::cmd::{parse_cmds, check_and_set_from_args, check_and_set_server_from_args};
use self::toml::{read_config, save_if_not_exists, append_to_default_config,
                 check_and_set_from_toml, check_and_set_servers_from_toml};

pub use self::proxy_config::ProxyConfig;
pub use self::running_config::RunningConfig as Config;

lazy_static! {
    pub static ref CONFIG: Config = init_config().unwrap_or_else(|e| {
        println!("{:?}", e);
        exit(1);
    });
}

pub type ConfigResult<T> = Result<T, ConfigError>;

pub enum ConfigError {
    MissServerMethod,
    MissServerPassword,
    MissServerAddress,
    MissServerPort,
    OpenFileFailed(String),
    ParseConfigFailed(String),
    InvalidMode(String),
    InvalidMethod(String),
    InvalidNumber(String),
    InvalidAddress(String),
    OutOfRange(i64),
    Other(String),
}

impl fmt::Debug for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            ConfigError::MissServerMethod => write!(f, "server method is missing"),
            ConfigError::MissServerPassword => write!(f, "server password is missing"),
            ConfigError::MissServerAddress => write!(f, "server address is missing"),
            ConfigError::MissServerPort => write!(f, "server port is missing"),
            ConfigError::OpenFileFailed(ref desc) => write!(f, "open config file failed: {}", desc),
            ConfigError::ParseConfigFailed(ref desc) => {
                write!(f, "parse config file error: {}", desc)
            }
            ConfigError::InvalidMode(ref desc) => write!(f, "invalid mode: {}", desc),
            ConfigError::InvalidMethod(ref desc) => {
                write!(f, "invalid encryption method: {}", desc)
            }
            ConfigError::InvalidNumber(ref desc) => write!(f, "invalid number: {}", desc),
            ConfigError::InvalidAddress(ref desc) => write!(f, "invalid address: {}", desc),
            ConfigError::OutOfRange(n) => write!(f, "{} is out of range", n),
            ConfigError::Other(ref desc) => write!(f, "{}", desc),
        }
    }
}

/// The working config follows a few rules:
/// 1. Command line is prior to config file.
/// 2. If no arguments provide, then read from default config file.
/// 3. If default config file doesn't exists, then randomly generated one and save it.
pub fn init_config() -> Result<Config, ConfigError> {
    let mut conf = Config::default();
    let default_config_path = Config::default_config_path();
    let args = parse_cmds();

    if cfg!(feature = "sslocal") {
        if let Some(server_conf) = args.value_of("add_server") {
            let mut tmp = Arc::make_mut(&mut conf.proxy_conf);
            tmp.base64_decode(server_conf)?;
            append_to_default_config(tmp);
            exit(0);
        }
        // TODO: share sslocal server according mode
        if args.is_present("share_server") {
            exit(0);
        }
    } else {
        // TODO: share ssserver server (check 0.0.0.0 & 127.0.0.1)
        if args.is_present("share_server") {
            exit(0);
        }
    }

    // setup from input and save it if no default config
    if let Some(input) = args.value_of("input") {
        let mut proxy_conf = ProxyConfig::default();
        proxy_conf.base64_decode(input)?;
        let proxy_conf = Arc::new(proxy_conf);
        if cfg!(feature = "sslocal") {
            conf.server_confs = Some(vec![proxy_conf]);
        } else {
            conf.proxy_conf = proxy_conf;
        }
        check_and_set_from_args(&args, &mut conf)?;
        save_if_not_exists(&conf);
        // setup from command line
    } else if args.value_of("server").is_some() {
        check_and_set_from_args(&args, &mut conf)?;
        check_and_set_server_from_args(&args, &mut conf)?;
        // setup from config file
    } else if args.value_of("config").is_some() || default_config_path.exists() {
        let config_path = match args.value_of("config") {
            Some(path) => PathBuf::from(path),
            None => default_config_path,
        };
        let tbl = read_config(&config_path)?;
        check_and_set_from_toml(&tbl, &mut conf)?;
        check_and_set_from_args(&args, &mut conf)?;
        // setup `server` or `servers`
        if conf.daemon != my_daemonize::Cmd::Stop {
            if cfg!(feature = "sslocal") {
                check_and_set_servers_from_toml(&tbl, &mut conf)?;
                println!("start sslocal with {}", config_path.display());
            } else {
                println!("start ssserver with {}", config_path.display());
            }
        }
        // create config if no args
    } else if !cfg!(feature = "sslocal") && args.args.is_empty() {
        {
            // set `address` to external ip
            let mut tmp = Arc::make_mut(&mut conf.proxy_conf);
            let ip = get_external_ip()?;
            tmp.set_address(Some(ip.as_str()))?;
        }
        println!("{}", conf.proxy_conf.base64_encode());
        save_if_not_exists(&conf);
    } else {
        check_and_set_from_args(&args, &mut conf)?;
    }

    if (conf.daemon == my_daemonize::Cmd::Start || conf.daemon == my_daemonize::Cmd::Restart) &&
       conf.log_file.is_none() {
        conf.log_file = Some(Config::default_log_path());
    }

    if cfg!(feature = "sslocal") && conf.server_confs.is_none() &&
       conf.daemon != my_daemonize::Cmd::Stop {
        return Err(ConfigError::MissServerAddress);
    }

    Ok(conf)
}

fn get_external_ip() -> ConfigResult<String> {
    const HOST_PATHS: &'static [(&'static str, &'static str)] = &[("ident.me", "/"),
                                                                  ("icanhazip.com", "/")];
    let mut external_ip = None;

    for host_path in HOST_PATHS {
        let ip = echo_ip(host_path.0, host_path.1);
        if ip.is_some() {
            external_ip = ip;
            break;
        }
    }

    match external_ip {
        Some(ip) => {
            if check_ip(&ip) {
                Ok(ip)
            } else {
                Err(ConfigError::Other("no external ip available".to_string()))
            }
        }
        None => Err(ConfigError::Other("cannot get external ip".to_string())),
    }
}

fn echo_ip(host: &str, path: &str) -> Option<String> {
    let addr = try_opt!((host, 80).to_socket_addrs().ok().and_then(|mut addrs| addrs.next()));
    let mut conn = try_opt!(TcpStream::connect(addr).ok());
    let r = format!("GET {} HTTP/1.1\r\nHost: {}\r\n\r\n", path, host);
    try_opt!(conn.write_all(r.as_bytes()).ok());
    let mut s = String::new();
    try_opt!(conn.read_to_string(&mut s).ok());

    // handle HTTP chunks
    let mut lines: Vec<&str> = s.trim().lines().collect();
    let mut ip = lines.pop();
    if ip == Some("0") {
        ip = lines.pop().map(|l| l.trim());
    }
    ip.map(|s| s.to_string())
}

fn get_all_ips() -> Option<String> {
    let cmd = if cfg!(windows) {
        "ipconfig"
    } else {
        "ifconfig"
    };

    let output = try_opt!(Command::new(cmd).output().ok());
    String::from_utf8(output.stdout).ok()
}

fn check_ip(ip: &str) -> bool {
    if let Some(ips) = get_all_ips() {
        ips.find(ip).is_some()
    } else {
        false
    }
}
