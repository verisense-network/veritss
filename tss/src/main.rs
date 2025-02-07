mod commands;
use common::Settings;
use coordinator_signer::crypto::validator_identity::p2p_identity::P2pIdentity;
use coordinator_signer::crypto::{PkId, ValidatorIdentity};
use coordinator_signer::node::Node;
use coordinator_signer::signer::Signer;
use coordinator_signer::{
    coordinator::Coordinator, crypto::validator_identity::ValidatorIdentityIdentity,
};
use rand::Rng;
use std::collections::HashSet;
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;
use std::{error::Error, fs::File, io::Read};
use tokio::time::Instant;
use tokio::{self, time};
use tracing;

pub fn load_keypair(path: &str) -> libp2p::identity::Keypair {
    let mut f = File::open(path).unwrap();
    // read the file into a buffer
    let mut buffer = Vec::new();
    f.read_to_end(&mut buffer).unwrap();
    libp2p::identity::Keypair::from_protobuf_encoding(buffer.as_slice()).unwrap()
}
pub(crate) fn random_readable_string(length: usize) -> String {
    let mut rng = rand::thread_rng();
    let mut bytes = Vec::with_capacity(length);
    for _ in 0..length {
        bytes.push(rng.gen::<u8>());
    }
    hex::encode(bytes)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // let home_dir = directories::UserDirs::new()
    //     .unwrap()
    //     .home_dir()
    //     .to_path_buf();
    let home_dir = PathBuf::from("");
    let home_dir = home_dir.join(".veritss");
    let _guard = common::init_logging(
        None,
        if Settings::global().logging.file.enable {
            Some(home_dir.clone())
        } else {
            None
        },
    );
    let coordinator_ip_addr = Settings::global()
        .coordinator
        .remote_addr
        .parse::<IpAddr>()
        .unwrap();
    let coordinator_port = Settings::global().coordinator.port;
    let coordinator_peer_id = Settings::global().coordinator.peer_id;
    let whitelist: HashSet<<P2pIdentity as ValidatorIdentity>::Identity> = Settings::global()
        .coordinator
        .peer_id_whitelist
        .iter()
        .map(|peer_id| <P2pIdentity as ValidatorIdentity>::Identity::from_fmt_str(peer_id).unwrap())
        .collect();
    let cmd = commands::parse_args();
    match cmd {
        commands::Commands::Coordinator => {
            // default keypair = 12D3KooWB3LpKiErRF3byUAsCvY6JL8TtQeSCrF5Hw23UoKJ7F88
            let keypair = load_keypair(Settings::global().coordinator.keypair_path.as_str());
            let coordinator = Coordinator::<P2pIdentity>::new(keypair, home_dir, Some(whitelist))?;
            // coordinator.start_listening().await?;
            coordinator.start_listening().await?;
        }
        commands::Commands::Signer { id } => {
            let keypair = load_keypair(
                Settings::global()
                    .signer
                    .keypair_path_mapping
                    .get(&id)
                    .unwrap(),
            );
            // convert id to peer id
            let peer_id = keypair.public().to_peer_id();
            tracing::info!("Starting signer with validator peer id: {}", peer_id);
            let signer = Signer::<P2pIdentity>::new(
                keypair,
                home_dir,
                coordinator_ip_addr,
                coordinator_port,
                coordinator_peer_id,
            )?;
            signer.start_listening().await?;
        }
        commands::Commands::DKG {
            min_signer,
            crypto_type,
        } => {
            let keypair = load_keypair(Settings::global().node.keypair_path.as_str());
            let mut node = Node::<P2pIdentity>::new(
                keypair,
                home_dir,
                coordinator_ip_addr,
                coordinator_port,
                coordinator_peer_id,
            )
            .await?;
            let participants = Settings::global()
                .coordinator
                .peer_id_whitelist
                .iter()
                .map(|peer_id| libp2p::identity::PeerId::from_fmt_str(peer_id).unwrap())
                .collect::<Vec<_>>();
            let resp = node
                .key_generate(crypto_type, participants, min_signer)
                .unwrap();
            let r = resp.await.unwrap().unwrap();
            tracing::info!("{:?}", r.to_string());
            println!("{:?}", r.to_string());
        }
        commands::Commands::Sign {
            pkid,
            message,
            tweak,
        } => {
            println!("pkid: {}", pkid);
            println!("message: {}", message);
            println!("tweak: {:?}", tweak);
            let keypair = load_keypair(Settings::global().node.keypair_path.as_str());
            let mut node = Node::<P2pIdentity>::new(
                keypair,
                home_dir,
                coordinator_ip_addr,
                coordinator_port,
                coordinator_peer_id,
            )
            .await?;
            let resp = node
                .sign(
                    PkId::new(hex::decode(&pkid).unwrap()),
                    message.as_bytes().to_vec(),
                    tweak.map(|t| t.as_bytes().to_vec()),
                )
                .unwrap();
            let r = resp.await.unwrap().unwrap();
            println!("{}", r.pretty_print());
            println!("{:?}", r._verify());
        }
        commands::Commands::LoopSign { pkid, times } => {
            let pkid = PkId::new(hex::decode(&pkid).unwrap());
            let tweak = random_readable_string(100);
            let keypair = load_keypair(Settings::global().node.keypair_path.as_str());
            let mut node = Node::<P2pIdentity>::new(
                keypair,
                home_dir,
                coordinator_ip_addr,
                coordinator_port,
                coordinator_peer_id,
            )
            .await?;
            let mut queue = Vec::new();
            let start = Instant::now();
            for _ in 0..times {
                let message = random_readable_string(100);
                let resp = node
                    .sign(
                        pkid.clone(),
                        message.clone().as_bytes().to_vec(),
                        Some(tweak.as_bytes().to_vec()),
                    )
                    .unwrap();
                queue.push((resp, message));
                time::sleep(Duration::from_millis(1)).await;
            }
            let mut count = 0;
            for (resp, message) in queue {
                tokio::select! {
                    _ = resp => {
                        count += 1;
                        println!("count: {}", count);
                    }
                    _ = time::sleep(Duration::from_millis(1000)) => {
                        println!("timeout, message: {}", message);
                    }
                };
            }
            let end = Instant::now();
            println!("time: {:?}", end - start);
        }
        commands::Commands::Lspk => {
            let keypair = load_keypair(Settings::global().node.keypair_path.as_str());
            let node = Node::<P2pIdentity>::new(
                keypair,
                home_dir,
                coordinator_ip_addr,
                coordinator_port,
                coordinator_peer_id,
            )
            .await?;
            let r = node.lspk_async().await.unwrap();
            for (k, v) in r {
                let v = v.iter().map(|pkid| pkid.to_string()).collect::<Vec<_>>();
                println!("{}: {:?}", k, v);
            }
        }
        commands::Commands::Pk { pkid, tweak } => {
            let keypair = load_keypair(Settings::global().node.keypair_path.as_str());
            let node = Node::<P2pIdentity>::new(
                keypair,
                home_dir,
                coordinator_ip_addr,
                coordinator_port,
                coordinator_peer_id,
            )
            .await?;
            let r = node
                .pk_async(
                    PkId::new(hex::decode(&pkid).unwrap()),
                    tweak.map(|t| t.as_bytes().to_vec()),
                )
                .await
                .unwrap();
            println!(
                "tweak: {:?},group_public_key_tweak: {:?}",
                r.tweak_data.map(hex::encode),
                hex::encode(r.group_public_key_tweak)
            );
        }
    }
    Ok(())
}
