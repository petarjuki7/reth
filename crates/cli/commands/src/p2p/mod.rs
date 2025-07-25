//! P2P Debugging tool

use std::{path::PathBuf, sync::Arc};

use crate::common::CliNodeTypes;
use alloy_eips::BlockHashOrNumber;
use backon::{ConstantBuilder, Retryable};
use clap::{Parser, Subcommand};
use reth_chainspec::{EthChainSpec, EthereumHardforks, Hardforks};
use reth_cli::chainspec::ChainSpecParser;
use reth_cli_util::{get_secret_key, hash_or_num_value_parser};
use reth_config::Config;
use reth_network::{BlockDownloaderProvider, NetworkConfigBuilder};
use reth_network_p2p::bodies::client::BodiesClient;
use reth_node_core::{
    args::{DatadirArgs, NetworkArgs},
    utils::get_single_header,
};

pub mod bootnode;
pub mod rlpx;

/// `reth p2p` command
#[derive(Debug, Parser)]
pub struct Command<C: ChainSpecParser> {
    #[command(subcommand)]
    command: Subcommands<C>,
}

impl<C: ChainSpecParser<ChainSpec: EthChainSpec + Hardforks + EthereumHardforks>> Command<C> {
    /// Execute `p2p` command
    pub async fn execute<N: CliNodeTypes<ChainSpec = C::ChainSpec>>(self) -> eyre::Result<()> {
        match self.command {
            Subcommands::Header { args, id } => {
                let handle = args.launch_network::<N>().await?;
                let fetch_client = handle.fetch_client().await?;
                let backoff = args.backoff();

                let header = (move || get_single_header(fetch_client.clone(), id))
                    .retry(backoff)
                    .notify(|err, _| println!("Error requesting header: {err}. Retrying..."))
                    .await?;
                println!("Successfully downloaded header: {header:?}");
            }

            Subcommands::Body { args, id } => {
                let handle = args.launch_network::<N>().await?;
                let fetch_client = handle.fetch_client().await?;
                let backoff = args.backoff();

                let hash = match id {
                    BlockHashOrNumber::Hash(hash) => hash,
                    BlockHashOrNumber::Number(number) => {
                        println!("Block number provided. Downloading header first...");
                        let client = fetch_client.clone();
                        let header = (move || {
                            get_single_header(client.clone(), BlockHashOrNumber::Number(number))
                        })
                        .retry(backoff)
                        .notify(|err, _| println!("Error requesting header: {err}. Retrying..."))
                        .await?;
                        header.hash()
                    }
                };
                let (_, result) = (move || {
                    let client = fetch_client.clone();
                    client.get_block_bodies(vec![hash])
                })
                .retry(backoff)
                .notify(|err, _| println!("Error requesting block: {err}. Retrying..."))
                .await?
                .split();
                if result.len() != 1 {
                    eyre::bail!(
                        "Invalid number of headers received. Expected: 1. Received: {}",
                        result.len()
                    )
                }
                let body = result.into_iter().next().unwrap();
                println!("Successfully downloaded body: {body:?}")
            }
            Subcommands::Rlpx(command) => {
                command.execute().await?;
            }
            Subcommands::Bootnode(command) => {
                command.execute().await?;
            }
        }

        Ok(())
    }
}

impl<C: ChainSpecParser> Command<C> {
    /// Returns the underlying chain being used to run this command
    pub fn chain_spec(&self) -> Option<&Arc<C::ChainSpec>> {
        match &self.command {
            Subcommands::Header { args, .. } => Some(&args.chain),
            Subcommands::Body { args, .. } => Some(&args.chain),
            Subcommands::Rlpx(_) => None,
            Subcommands::Bootnode(_) => None,
        }
    }
}

/// `reth p2p` subcommands
#[derive(Subcommand, Debug)]
pub enum Subcommands<C: ChainSpecParser> {
    /// Download block header
    Header {
        #[command(flatten)]
        args: DownloadArgs<C>,
        /// The header number or hash
        #[arg(value_parser = hash_or_num_value_parser)]
        id: BlockHashOrNumber,
    },
    /// Download block body
    Body {
        #[command(flatten)]
        args: DownloadArgs<C>,
        /// The block number or hash
        #[arg(value_parser = hash_or_num_value_parser)]
        id: BlockHashOrNumber,
    },
    // RLPx utilities
    Rlpx(rlpx::Command),
    /// Bootnode command
    Bootnode(bootnode::Command),
}

#[derive(Debug, Clone, Parser)]
pub struct DownloadArgs<C: ChainSpecParser> {
    /// The number of retries per request
    #[arg(long, default_value = "5")]
    retries: usize,

    #[command(flatten)]
    network: NetworkArgs,

    #[command(flatten)]
    datadir: DatadirArgs,

    /// The path to the configuration file to use.
    #[arg(long, value_name = "FILE", verbatim_doc_comment)]
    config: Option<PathBuf>,

    /// The chain this node is running.
    ///
    /// Possible values are either a built-in chain or the path to a chain specification file.
    #[arg(
        long,
        value_name = "CHAIN_OR_PATH",
        long_help = C::help_message(),
        default_value = C::SUPPORTED_CHAINS[0],
        value_parser = C::parser()
    )]
    chain: Arc<C::ChainSpec>,
}

impl<C: ChainSpecParser> DownloadArgs<C> {
    /// Creates and spawns the network and returns the handle.
    pub async fn launch_network<N>(
        &self,
    ) -> eyre::Result<reth_network::NetworkHandle<N::NetworkPrimitives>>
    where
        C::ChainSpec: EthChainSpec + Hardforks + EthereumHardforks + Send + Sync + 'static,
        N: CliNodeTypes<ChainSpec = C::ChainSpec>,
    {
        let data_dir = self.datadir.clone().resolve_datadir(self.chain.chain());
        let config_path = self.config.clone().unwrap_or_else(|| data_dir.config());

        // Load configuration
        let mut config = Config::from_path(&config_path).unwrap_or_default();

        config.peers.trusted_nodes.extend(self.network.trusted_peers.clone());

        if config.peers.trusted_nodes.is_empty() && self.network.trusted_only {
            eyre::bail!(
                "No trusted nodes. Set trusted peer with `--trusted-peer <enode record>` or set `--trusted-only` to `false`"
            )
        }

        config.peers.trusted_nodes_only = self.network.trusted_only;

        let default_secret_key_path = data_dir.p2p_secret();
        let secret_key_path =
            self.network.p2p_secret_key.clone().unwrap_or(default_secret_key_path);
        let p2p_secret_key = get_secret_key(&secret_key_path)?;
        let rlpx_socket = (self.network.addr, self.network.port).into();
        let boot_nodes = self.chain.bootnodes().unwrap_or_default();

        let net = NetworkConfigBuilder::<N::NetworkPrimitives>::new(p2p_secret_key)
            .peer_config(config.peers_config_with_basic_nodes_from_file(None))
            .external_ip_resolver(self.network.nat)
            .boot_nodes(boot_nodes.clone())
            .apply(|builder| {
                self.network.discovery.apply_to_builder(builder, rlpx_socket, boot_nodes)
            })
            .build_with_noop_provider(self.chain.clone())
            .manager()
            .await?;
        let handle = net.handle().clone();
        tokio::task::spawn(net);

        Ok(handle)
    }

    pub fn backoff(&self) -> ConstantBuilder {
        ConstantBuilder::default().with_max_times(self.retries.max(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reth_ethereum_cli::chainspec::EthereumChainSpecParser;

    #[test]
    fn parse_header_cmd() {
        let _args: Command<EthereumChainSpecParser> =
            Command::parse_from(["reth", "header", "--chain", "mainnet", "1000"]);
    }

    #[test]
    fn parse_body_cmd() {
        let _args: Command<EthereumChainSpecParser> =
            Command::parse_from(["reth", "body", "--chain", "mainnet", "1000"]);
    }
}
