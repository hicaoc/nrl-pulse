//! FMO 协议支持（移植自 open-fmo/sim-rust）。
//! 部分底层密码学/编解码工具函数为完整协议实现保留，暂未被调用，允许 dead_code。
#![allow(dead_code)]

pub mod activate;
pub mod aprs;
pub mod audio;
pub mod broadcast;
pub mod certstore;
pub mod fmo_auth;
pub mod fmo_frame;
pub mod ima_adpcm;
pub mod mqtt_client;
pub mod presence;
pub mod protocol;
pub mod qso;
pub mod state;

