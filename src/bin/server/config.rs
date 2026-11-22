//! Where the data comes from and where the server listens, from the command line.

use std::path::PathBuf;

/// The address used when neither `--unix` nor `--tcp` is given.
const DEFAULT_TCP: &str = "127.0.0.1:7600";


pub(crate) struct Config {
    pub(crate) schema: PathBuf,
    pub(crate) data: PathBuf,
    pub(crate) unix_socket: Option<PathBuf>,
    pub(crate) tcp_address: Option<String>,
}


impl Config {
    /// `--schema <file> --data <dir> [--unix <path>] [--tcp <host:port>]`
    pub(crate) fn from_args(mut args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut schema = None;
        let mut data = None;
        let mut unix_socket = None;
        let mut tcp_address = None;

        while let Some(flag) = args.next() {
            let value = args.next().ok_or(format!("{flag} needs a value"))?;

            match flag.as_str() {
                "--schema" => schema = Some(PathBuf::from(value)),
                "--data" => data = Some(PathBuf::from(value)),
                "--unix" => unix_socket = Some(PathBuf::from(value)),
                "--tcp" => tcp_address = Some(value),
                other => return Err(format!("unknown argument {other}")),
            }
        }

        if unix_socket.is_none() && tcp_address.is_none() {
            tcp_address = Some(DEFAULT_TCP.to_string());
        }

        Ok(Self {
            schema: schema.ok_or("--schema is required")?,
            data: data.ok_or("--data is required")?,
            unix_socket,
            tcp_address,
        })
    }
}
