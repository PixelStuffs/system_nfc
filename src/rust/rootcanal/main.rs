// Copyright 2021, The Android Open Source Project
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! This connects to "rootcanal" and provides a simulated
//! Nfc chip as well as a simulated environment.

use bytes::{BufMut, BytesMut};
use log::{debug, Level};
use logger::{self, Config};
use nfc_packets::nci;
use nfc_packets::nci::{CommandChild, NciChild};
use nfc_packets::nci::{
    ConfigStatus, NciVersion, ResetNotificationBuilder, ResetResponseBuilder, ResetTrigger,
    ResetType,
};
use nfc_packets::nci::{InitResponseBuilder, NfccFeatures, RfInterface};
use nfc_packets::nci::{NciMsgType, NciPacket, Packet, PacketBoundaryFlag};
use std::convert::TryInto;
use thiserror::Error;
use tokio::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, ErrorKind};
use tokio::net::TcpListener;

/// Result type
type Result<T> = std::result::Result<T, RootcanalError>;

#[derive(Debug, Error)]
enum RootcanalError {
    #[error("Termination request")]
    TerminateTask,
    #[error("Socket error")]
    IoError(#[from] io::Error),
    #[error("Unsupported command packet")]
    UnsupportedCommand,
    #[error("Packet did not parse correctly")]
    InvalidPacket,
    #[error("Packet type not supported")]
    UnsupportedPacket,
}

const TERMINATION: u8 = 4u8;

#[tokio::main]
async fn main() -> io::Result<()> {
    logger::init(Config::default().with_tag_on_device("nfc-rc").with_min_level(Level::Trace));

    let listener = TcpListener::bind("127.0.0.1:54323").await?;

    for _ in 0..2 {
        let (mut sock, _) = listener.accept().await?;

        tokio::spawn(async move {
            let (rd, mut wr) = sock.split();
            let mut rd = BufReader::new(rd);
            loop {
                if let Err(e) = process(&mut rd, &mut wr).await {
                    match e {
                        RootcanalError::TerminateTask => break,
                        RootcanalError::IoError(e) => {
                            if e.kind() == ErrorKind::UnexpectedEof {
                                break;
                            }
                        }
                        _ => panic!("Communication error: {:?}", e),
                    }
                }
            }
        })
        .await?;
    }
    Ok(())
}

async fn process<R, W>(reader: &mut R, writer: &mut W) -> Result<()>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    let mut buffer = BytesMut::with_capacity(1024);
    let len: usize = reader.read_u16().await?.into();
    buffer.resize(len, 0);
    reader.read_exact(&mut buffer).await?;
    let frozen = buffer.freeze();
    debug!("{:?}", &frozen);
    let pkt_type = (frozen[0] >> 5) & 0x7;
    debug!("packet {} received len={}", &pkt_type, &len);
    if pkt_type == NciMsgType::Command as u8 {
        match NciPacket::parse(&frozen) {
            Ok(p) => command_response(writer, p).await,
            Err(_) => Err(RootcanalError::InvalidPacket),
        }
    } else if pkt_type == TERMINATION {
        Err(RootcanalError::TerminateTask)
    } else {
        Err(RootcanalError::UnsupportedPacket)
    }
}

async fn command_response<W>(out: &mut W, cmd: NciPacket) -> Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    let pbf = PacketBoundaryFlag::CompleteOrFinal;
    let gid = 0u8;
    match cmd.specialize() {
        NciChild::Command(cmd) => match cmd.specialize() {
            CommandChild::ResetCommand(rst) => {
                write_nci(
                    out,
                    (ResetResponseBuilder { gid, pbf, status: nci::Status::Ok }).build(),
                )
                .await?;
                write_nci(
                    out,
                    (ResetNotificationBuilder {
                        gid,
                        pbf,
                        trigger: ResetTrigger::ResetCommand,
                        config_status: if rst.get_reset_type() == ResetType::KeepConfig {
                            ConfigStatus::ConfigKept
                        } else {
                            ConfigStatus::ConfigReset
                        },
                        nci_version: NciVersion::Version20,
                        manufacturer_id: 0,
                        mfsi: Vec::new(),
                    })
                    .build(),
                )
                .await
            }
            CommandChild::InitCommand(_) => {
                let nfcc_feat = [0u8; 5];
                let rf_int = [0u8; 2];
                write_nci(
                    out,
                    (InitResponseBuilder {
                        gid,
                        pbf,
                        status: nci::Status::Ok,
                        nfcc_features: NfccFeatures::parse(&nfcc_feat).unwrap(),
                        max_log_conns: 0,
                        max_rout_tbls_size: 0x0000,
                        max_ctrl_payload: 255,
                        max_data_payload: 255,
                        num_of_credits: 0,
                        max_nfcv_rf_frame_sz: 64,
                        rf_interface: vec![RfInterface::parse(&rf_int).unwrap(); 1],
                    })
                    .build(),
                )
                .await
            }
            _ => Err(RootcanalError::UnsupportedCommand),
        },
        _ => Err(RootcanalError::InvalidPacket),
    }
}

async fn write_nci<W, T>(writer: &mut W, rsp: T) -> Result<()>
where
    W: AsyncWriteExt + Unpin,
    T: Into<NciPacket>,
{
    let pkt = rsp.into();
    let b = pkt.to_bytes();
    let mut data = BytesMut::with_capacity(b.len() + 2);
    data.put_u16(b.len().try_into().unwrap());
    data.extend(b);
    let frozen = data.freeze();
    writer.write_all(frozen.as_ref()).await?;
    debug!("command written");
    Ok(())
}
