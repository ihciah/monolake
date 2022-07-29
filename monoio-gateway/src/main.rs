#![feature(generic_associated_types)]
#![feature(type_alias_impl_trait)]

use anyhow::{Ok, Result};

pub mod balance;
pub mod config;
pub mod discover;
pub mod dns;
pub mod gateway;
pub mod http;
pub mod layer;
pub mod proxy;

#[monoio::main(timer_enabled = true)]
async fn main() -> Result<()> {
    Ok(())
}
