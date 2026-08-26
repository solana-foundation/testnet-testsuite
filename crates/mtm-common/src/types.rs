use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Instrument identifier. For pairs this is "BASE/QUOTE" (uppercase by
/// convention); for custom testnet tokens it is the mint address itself —
/// so it is CASE-SENSITIVE and never normalized (base58 would corrupt).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Symbol(String);

impl Symbol {
    pub fn new(s: impl AsRef<str>) -> Self {
        Self(s.as_ref().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for Symbol {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::new(s))
    }
}

/// Target Solana cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Cluster {
    Local,
    Devnet,
    Testnet,
    MainnetBeta,
}

impl Cluster {
    pub fn default_rpc_url(&self) -> &'static str {
        match self {
            Cluster::Local => "http://127.0.0.1:8899",
            Cluster::Devnet => "https://api.devnet.solana.com",
            Cluster::Testnet => "https://api.testnet.solana.com",
            Cluster::MainnetBeta => "https://api.mainnet-beta.solana.com",
        }
    }
}

impl fmt::Display for Cluster {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Cluster::Local => "local",
            Cluster::Devnet => "devnet",
            Cluster::Testnet => "testnet",
            Cluster::MainnetBeta => "mainnet-beta",
        };
        f.write_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_preserves_case() {
        // mint addresses are symbols — base58 must survive untouched
        assert_eq!(
            Symbol::new("7pMcAg9x3GJqUxWZcntjZiy5UJPXfPZFoVwuCPCBpMcx").as_str(),
            "7pMcAg9x3GJqUxWZcntjZiy5UJPXfPZFoVwuCPCBpMcx"
        );
    }
}
