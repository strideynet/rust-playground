use serde::{Serialize, Deserialize};
use color_eyre::eyre::Result;

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct Config {
    pub metrics_listen_addr: String,
    pub listeners: Vec<ListenerConfig>,
}

impl Config {
    pub fn load() -> Result <Config> {
        let cfg: Config = figment::Figment::from(
            figment::providers::Serialized::defaults(Config::default())
        ).extract()?;
        Ok(cfg)
    }
}

impl Default for Config {
    fn default() -> Config {
        Config {
            metrics_listen_addr: "127.0.0.1:3884".into(),
            listeners: vec![],
        }
    }
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
pub struct ListenerConfig {
    pub listen_addr: String,
    pub upstream: ListenerUpstreamConfig,
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
#[serde(tag = "type")]
pub enum ListenerUpstreamConfig {
    #[serde(rename = "tcp")]
    TCP(TCPUpstreamConfig),
    #[serde(rename = "tls")]
    TLS(TLSUpstreamConfig)
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
pub struct TCPUpstreamConfig {
    pub addr: String,
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
pub struct TLSUpstreamConfig {
    pub addr: String,
}

#[cfg(test)]
mod tests {
    use figment::providers::Format;

    // Note this useful idiom: importing names from outer (for mod tests) scope.
    use super::*;

    #[test]
    fn test_config_load() {
        let cfg: Config = figment::Figment::from(
            figment::providers::Serialized::defaults(Config::default())
        ).merge(figment::providers::Yaml::file("./src/testdata/config.yaml"))
        .extract().unwrap();
        assert_eq!(
            cfg,
            Config{
                metrics_listen_addr: "0.0.0.0:1337".into(),
                listeners: vec![
                    ListenerConfig{
                        listen_addr: "0.0.0.0:1338".into(),
                        upstream: ListenerUpstreamConfig::TCP(
                            TCPUpstreamConfig {
                                addr: "127.0.0.1:8080".into(),
                            }
                        )
                    }
                ]
            }
        )
    }
}