// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::utils;
use config::config::{NodeConfig, RoleType};
use config_builder::swarm_config::{SwarmConfig, SwarmConfigBuilder};
use crypto::{ed25519::*, test_utils::KeyPair};
use debug_interface::NodeDebugClient;
use failure::prelude::*;
use logger::prelude::*;
use std::{
    collections::HashMap,
    env,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command},
    str::FromStr,
};
use tools::tempdir::TempPath;

const LIBRA_NODE_BIN: &str = "libra_node";

pub struct LibraNode {
    node: Child,
    debug_client: NodeDebugClient,
    ac_port: u16,
    peer_id: String,
    log: PathBuf,
}

impl Drop for LibraNode {
    // When the LibraNode struct goes out of scope we need to kill the child process
    fn drop(&mut self) {
        // check if the process has already been terminated
        match self.node.try_wait() {
            // The child process has already terminated, perhaps due to a crash
            Ok(Some(_)) => {}

            // The node is still running so we need to attempt to kill it
            _ => {
                if let Err(e) = self.node.kill() {
                    panic!("LibraNode process could not be killed: '{}'", e);
                }
            }
        }
    }
}

impl LibraNode {
    pub fn launch(
        config: &NodeConfig,
        config_path: &Path,
        logdir: &Path,
        disable_logging: bool,
    ) -> Result<Self> {
        // For now, We consider the peer id on the first network config as the node's peer id.
        // TODO: Create a peer id independent node identifier.
        let peer_id = config.networks.get(0).unwrap().peer_id.clone();
        let log = logdir.join(format!("{}.log", SwarmConfig::get_alias(&config)));
        let log_file = File::create(&log)?;
        let mut node_command = Command::new(utils::get_bin(LIBRA_NODE_BIN));
        node_command
            .current_dir(utils::workspace_root())
            .arg("-f")
            .arg(config_path)
            .args(&["-p", &peer_id]);
        if env::var("RUST_LOG").is_err() {
            // Only set our RUST_LOG if its not present in environment
            node_command.env("RUST_LOG", "debug");
        }
        if disable_logging {
            node_command.arg("-d");
        }

        node_command
            .stdout(log_file.try_clone()?)
            .stderr(log_file.try_clone()?);

        let node = node_command
            .spawn()
            .context("Error launching node process")?;

        let debug_client = NodeDebugClient::new(
            "localhost",
            config.debug_interface.admission_control_node_debug_port,
        );
        Ok(Self {
            node,
            debug_client,
            ac_port: config.admission_control.admission_control_service_port,
            peer_id,
            log,
        })
    }

    pub fn peer_id(&self) -> String {
        self.peer_id.clone()
    }

    pub fn ac_port(&self) -> u16 {
        self.ac_port
    }

    pub fn get_log_contents(&self) -> Result<String> {
        let mut log = File::open(&self.log)?;
        let mut contents = String::new();
        log.read_to_string(&mut contents)?;

        Ok(contents)
    }

    fn get_metric(&self, metric_name: &str) -> Option<i64> {
        match self.debug_client.get_node_metric(metric_name) {
            Err(e) => {
                debug!(
                    "error getting {} for node: {}; error: {}",
                    metric_name, self.peer_id, e
                );
                None
            }
            Ok(maybeval) => {
                if maybeval.is_none() {
                    debug!("Node: {} did not report {}", self.peer_id, metric_name);
                }
                maybeval
            }
        }
    }

    pub fn check_connectivity(&self, expected_peers: i64) -> bool {
        if let Some(num_connected_peers) = self.get_metric("network_gauge{op=connected_peers}") {
            if num_connected_peers != expected_peers {
                debug!(
                    "Node '{}' Expected peers: {}, found peers: {}",
                    self.peer_id, expected_peers, num_connected_peers
                );
                return false;
            } else {
                return true;
            }
        }
        false
    }

    pub fn health_check(&mut self) -> HealthStatus {
        debug!("Health check on node '{}'", self.peer_id);

        // check if the process has terminated
        match self.node.try_wait() {
            // This would mean the child process has crashed
            Ok(Some(status)) => {
                debug!("Node '{}' crashed with: {}", self.peer_id, status);
                return HealthStatus::Crashed(status);
            }

            // This is the case where the node is still running
            Ok(None) => {}

            // Some other unknown error
            Err(e) => {
                panic!("error attempting to query Node: {}", e);
            }
        }

        match self.debug_client.get_node_metrics() {
            Ok(_) => {
                debug!("Node '{}' is healthy", self.peer_id);
                HealthStatus::Healthy
            }
            Err(e) => {
                debug!("Error querying metrics for node '{}'", self.peer_id);
                HealthStatus::RpcFailure(e)
            }
        }
    }
}

pub enum HealthStatus {
    Healthy,
    Crashed(::std::process::ExitStatus),
    RpcFailure(failure::Error),
}

/// A wrapper that unifies PathBuf and TempPath.
#[derive(Debug)]
pub enum LibraSwarmDir {
    Persistent(PathBuf),
    Temporary(TempPath),
}

impl AsRef<Path> for LibraSwarmDir {
    fn as_ref(&self) -> &Path {
        match self {
            LibraSwarmDir::Persistent(path_buf) => path_buf.as_path(),
            LibraSwarmDir::Temporary(temp_dir) => temp_dir.path(),
        }
    }
}

/// Struct holding instances and information of Libra Swarm
pub struct LibraSwarm {
    // Output log, LibraNodes' config file, libradb etc, into this dir.
    pub dir: Option<LibraSwarmDir>,
    // Maps the peer id of a node to the LibraNode struct
    pub validator_nodes: HashMap<String, LibraNode>,
    pub full_nodes: Vec<LibraNode>,
    pub config: SwarmConfig,
}

#[derive(Debug, Fail)]
pub enum SwarmLaunchFailure {
    /// Timeout while waiting for nodes to start
    #[fail(display = "Node launch check timeout")]
    LaunchTimeout,
    /// Node return status indicates a crash
    #[fail(display = "Node crash")]
    NodeCrash,
    /// Timeout while waiting for the nodes to report that they're all interconnected
    #[fail(display = "Node connectivity check timeout")]
    ConnectivityTimeout,
}

impl LibraSwarm {
    pub fn launch_swarm(
        num_nodes: usize,
        disable_logging: bool,
        faucet_account_keypair: KeyPair<Ed25519PrivateKey, Ed25519PublicKey>,
        config_dir: Option<String>,
        template_path: Option<String>,
    ) -> Self {
        let num_launch_attempts = 5;
        for i in 0..num_launch_attempts {
            let swarm_config_dir = Self::setup_config_dir(&config_dir);
            info!("Launch swarm attempt: {} of {}", i, num_launch_attempts);
            match Self::launch_swarm_attempt(
                num_nodes,
                disable_logging,
                faucet_account_keypair.clone(),
                swarm_config_dir,
                &template_path,
            ) {
                Ok(swarm) => {
                    return swarm;
                }
                Err(e) => error!("Error launching swarm: {}", e),
            }
        }
        panic!("Max out {} attempts to launch swarm", num_launch_attempts);
    }

    /// Either create a persistent directory for swarm or return a temporary one.
    /// If specified persistent directory already exists,
    /// assumably due to previous launch failure, it will be removed.
    /// The directory for the last failed attempt won't be removed.
    fn setup_config_dir(config_dir: &Option<String>) -> LibraSwarmDir {
        let dir = match config_dir {
            Some(dir_str) => {
                let path_buf = PathBuf::from_str(&dir_str).expect("unable to create config dir");
                if path_buf.exists() {
                    std::fs::remove_dir_all(dir_str).expect("unable to delete previous config dir");
                }
                std::fs::create_dir_all(dir_str).expect("unable to create config dir");
                LibraSwarmDir::Persistent(path_buf)
            }
            None => {
                let temp_dir = TempPath::new();
                temp_dir
                    .create_as_dir()
                    .expect("unable to create temporary config dir");
                LibraSwarmDir::Temporary(temp_dir)
            }
        };
        println!("Base directory containing logs and configs: {:?}", &dir);
        dir
    }

    fn launch_swarm_attempt(
        num_nodes: usize,
        disable_logging: bool,
        faucet_account_keypair: KeyPair<Ed25519PrivateKey, Ed25519PublicKey>,
        dir: LibraSwarmDir,
        template_path: &Option<String>,
    ) -> std::result::Result<Self, SwarmLaunchFailure> {
        let logs_dir_path = dir.as_ref().join("logs");
        std::fs::create_dir(&logs_dir_path).unwrap();
        let base = utils::workspace_root().join(
            template_path
                .as_ref()
                .unwrap_or(&"config/data/configs/node.config.toml".to_string()),
        );
        let mut config_builder = SwarmConfigBuilder::new();

        config_builder
            .with_ipv4()
            .with_num_nodes(num_nodes)
            .with_base(base)
            .with_output_dir(&dir)
            .with_faucet_keypair(faucet_account_keypair);
        let config = config_builder.build().unwrap();

        let mut swarm = Self {
            dir: Some(dir),
            validator_nodes: HashMap::new(),
            full_nodes: vec![],
            config,
        };
        // For each config launch a node
        for (path, node_config) in &swarm.config.configs {
            let node =
                LibraNode::launch(&node_config, &path, &logs_dir_path, disable_logging).unwrap();
            if node_config.is_validator() {
                swarm.validator_nodes.insert(node.peer_id(), node);
            } else {
                swarm.full_nodes.push(node);
            }
        }

        swarm.wait_for_startup()?;
        swarm.wait_for_connectivity()?;

        info!("Successfully launched Swarm");

        Ok(swarm)
    }

    fn wait_for_connectivity(&self) -> std::result::Result<(), SwarmLaunchFailure> {
        // Early return if we're only launching a single node
        if self.validator_nodes.len() == 1 {
            return Ok(());
        }

        let num_attempts = 60;

        for i in 0..num_attempts {
            debug!("Wait for connectivity attempt: {}", i);

            if self
                .validator_nodes
                .values()
                .all(|node| node.check_connectivity(self.validator_nodes.len() as i64 - 1))
            {
                return Ok(());
            }
            // TODO check full node connectivity for full nodes

            ::std::thread::sleep(::std::time::Duration::from_millis(1000));
        }

        Err(SwarmLaunchFailure::ConnectivityTimeout)
    }

    fn wait_for_startup(&mut self) -> std::result::Result<(), SwarmLaunchFailure> {
        let num_attempts = 120;
        let mut done = vec![false; self.validator_nodes.len() + self.full_nodes.len()];
        for i in 0..num_attempts {
            debug!("Wait for startup attempt: {} of {}", i, num_attempts);
            for (node, done) in self
                .validator_nodes
                .values_mut()
                .chain(self.full_nodes.iter_mut())
                .zip(done.iter_mut())
            {
                if *done {
                    continue;
                }

                match node.health_check() {
                    HealthStatus::Healthy => *done = true,
                    HealthStatus::RpcFailure(_) => continue,
                    HealthStatus::Crashed(status) => {
                        error!(
                            "Libra node '{}' has crashed with status '{}'. Log output: '''{}'''",
                            node.peer_id,
                            status,
                            node.get_log_contents().unwrap()
                        );
                        return Err(SwarmLaunchFailure::NodeCrash);
                    }
                }
            }

            // Check if all the nodes have been successfully launched
            if done.iter().all(|status| *status) {
                return Ok(());
            }

            ::std::thread::sleep(::std::time::Duration::from_millis(1000));
        }

        Err(SwarmLaunchFailure::LaunchTimeout)
    }

    /// This function first checks the last committed round of all the nodes, picks the max
    /// value and then waits for all the nodes to catch up to that round.
    /// Once done, we can guarantee that all the txns committed before the invocation of this
    /// function are now available at all the nodes.
    pub fn wait_for_all_nodes_to_catchup(&mut self) -> bool {
        let num_attempts = 60;
        let last_committed_round_str = "consensus{op=committed_blocks_count}";
        let mut done = vec![false; self.validator_nodes.len()];

        let mut last_committed_round = 0;
        // First, try to retrieve the max value across all the committed rounds
        debug!("Calculating max committed round across the validators.");
        for node in self.validator_nodes.values() {
            match node.get_metric(last_committed_round_str) {
                Some(val) => {
                    debug!("\tNode {} last committed round = {}", node.peer_id, val);
                    last_committed_round = last_committed_round.max(val);
                }
                None => {
                    debug!(
                        "\tNode {} last committed round unknown, assuming 0.",
                        node.peer_id
                    );
                }
            }
        }

        // Now wait for all the nodes to catch up to the max.
        for i in 0..num_attempts {
            debug!(
                "Wait for catchup, target_commit_round = {}, attempt: {} of {}",
                last_committed_round,
                i + 1,
                num_attempts
            );
            for (node, done) in self.validator_nodes.values_mut().zip(done.iter_mut()) {
                if *done {
                    continue;
                }

                match node.get_metric(last_committed_round_str) {
                    Some(val) => {
                        if val >= last_committed_round {
                            debug!(
                                "\tNode {} is caught up with last committed round {}",
                                node.peer_id, val
                            );
                            *done = true;
                        } else {
                            debug!(
                                "\tNode {} is not caught up yet with last committed round {}",
                                node.peer_id, val
                            );
                        }
                    }
                    None => {
                        debug!(
                            "\tNode {} last committed round unknown, assuming 0.",
                            node.peer_id
                        );
                    }
                }
            }

            // Check if all the nodes have been successfully caught up
            if done.iter().all(|status| *status) {
                return true;
            }

            ::std::thread::sleep(::std::time::Duration::from_millis(1000));
        }

        false
    }

    /// A specific public AC port of a validator or a full node.
    pub fn get_ac_port(&self, index: usize, role: RoleType) -> u16 {
        match role {
            RoleType::Validator => *self
                .validator_nodes
                .values()
                .map(|node| node.ac_port())
                .collect::<Vec<u16>>()
                .get(index)
                .unwrap(),
            RoleType::FullNode => *self
                .full_nodes
                .iter()
                .map(|node| node.ac_port())
                .collect::<Vec<u16>>()
                .get(index)
                .unwrap(),
        }
    }

    /// Vector with the peer ids of the validators in the swarm.
    pub fn get_validators_ids(&self) -> Vec<String> {
        self.validator_nodes.keys().cloned().collect()
    }

    /// Vector with the debug ports of all the validators in the swarm.
    pub fn get_validators_debug_ports(&self) -> Vec<u16> {
        self.config
            .configs
            .iter()
            .map(|(_, c)| c.debug_interface.admission_control_node_debug_port)
            .collect()
    }

    pub fn get_validator(&self, peer_id: &str) -> Option<&LibraNode> {
        self.validator_nodes.get(peer_id)
    }

    pub fn kill_node(&mut self, peer_id: &str) {
        self.validator_nodes.remove(peer_id);
    }

    pub fn add_node(
        &mut self,
        peer_id: String,
        disable_logging: bool,
    ) -> std::result::Result<(), SwarmLaunchFailure> {
        // First take the configs out to not keep immutable borrow on self when calling
        // `launch_node`.
        self.launch_node(peer_id, disable_logging)
    }

    fn launch_node(
        &mut self,
        peer_id: String,
        disable_logging: bool,
    ) -> std::result::Result<(), SwarmLaunchFailure> {
        let (path, config) = self
            .config
            .configs
            .iter()
            .find(|(_path, config)| config.networks.get(0).unwrap().peer_id == peer_id)
            .expect(
                &format!(
                    "PeerId {} not found in any of the admission control service ports.",
                    peer_id
                )[..],
            );
        let logs_dir_path = self.dir.as_ref().map(|x| x.as_ref().join("logs")).unwrap();
        let mut node = LibraNode::launch(config, path, &logs_dir_path, disable_logging).unwrap();
        for _ in 0..60 {
            if let HealthStatus::Healthy = node.health_check() {
                self.validator_nodes.insert(peer_id, node);
                return self.wait_for_connectivity();
            }
            ::std::thread::sleep(::std::time::Duration::from_millis(1000));
        }
        Err(SwarmLaunchFailure::LaunchTimeout)
    }

    pub fn get_trusted_peers_config_path(&self) -> String {
        let (path, _) = &self.config.consensus_peers;
        path.canonicalize()
            .expect("Unable to get canonical path of trusted peers config file")
            .to_str()
            .unwrap()
            .to_string()
    }
}

impl Drop for LibraSwarm {
    fn drop(&mut self) {
        // If panicking, we don't want to gc the swarm directory.
        if std::thread::panicking() {
            if let Some(dir) = self.dir.take() {
                if let LibraSwarmDir::Temporary(temp_dir) = dir {
                    let log_path = temp_dir.path();
                    println!("logs located at {:?}", log_path);

                    // Dump logs for each validator to stdout when `LIBRA_DUMP_LOGS`
                    // environment variable is set
                    if env::var_os("LIBRA_DUMP_LOGS").is_some() {
                        for (peer_id, node) in &mut self.validator_nodes {
                            // Skip dumping logs for healthy nodes
                            if let HealthStatus::Healthy = node.health_check() {
                                continue;
                            }

                            // Grab the contents of the node's logs and skip if we were unable to
                            // grab its logs
                            let log_contents = match node.get_log_contents() {
                                Ok(contents) => contents,
                                Err(_) => continue,
                            };

                            println!();
                            println!();
                            println!("{:=^80}", "");
                            println!("Validator {}", peer_id);
                            println!();
                            println!();
                            println!("{}", log_contents);
                        }
                    }
                }
            }
        }
    }
}
