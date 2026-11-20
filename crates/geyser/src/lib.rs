//! The mtx runtime as a geyser plugin
//! (`docs/rfd/0004-strategies-on-mainnet.md`).
//!
//! A validator (or surfpool) loads this library, and calls it on its own
//! threads for every account written, every slot and every block. The
//! plugin does as little as it can there: it drops what the runtime
//! doesn't watch, copies the rest, and hands it over without waiting
//! (`mtx_runtime::live::feed`). The runtime has a thread of its own.
//!
//! Its configuration is the file the host loads the plugin from: the
//! library's path, as every geyser plugin's, and an `mtx` member
//! (`mtx_runtime::live::Config`):
//!
//! ```json
//! { "libpath": "libmtx_geyser.dylib",
//!   "mtx": { "journal": "/var/log/mtx/journal.jsonl",
//!            "backfill_rpc_url": "http://127.0.0.1:8899",
//!            "markets": [ { "exchange": "exchange.json", "symbol": "SPCX",
//!                           "authority": "<base58>" } ] } }
//! ```
//!
//! A panic on a validator's thread would take the validator down, so every
//! callback catches its own.

use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
};

use agave_geyser_plugin_interface::geyser_plugin_interface::{
    GeyserPlugin, GeyserPluginError, ReplicaAccountInfoVersions, ReplicaBlockInfoVersions,
    Result as PluginResult, SlotStatus as HostSlotStatus,
};
use mtx_core::feed::{AccountWrite, Address, FeedEvent, SlotStatus};
use mtx_runtime::live::{Config, Runtime, feed::FeedSender};

/// The name the host lists the plugin under.
pub const NAME: &str = "mtx-geyser";

/// The plugin: nothing until it is loaded, then the runtime and its feed.
#[derive(Debug, Default)]
pub struct MtxGeyser {
    running: Option<Running>,
}

#[derive(Debug)]
struct Running {
    feed: FeedSender,
    runtime: Runtime,
}

impl MtxGeyser {
    /// The feed, in a callback: an error before the plugin is loaded, which
    /// a host never does.
    fn feed(&self) -> PluginResult<&FeedSender> {
        self.running
            .as_ref()
            .map(|running| &running.feed)
            .ok_or_else(|| custom("the plugin isn't loaded"))
    }
}

impl GeyserPlugin for MtxGeyser {
    fn name(&self) -> &'static str {
        NAME
    }

    fn on_load(&mut self, config_file: &str, _is_reload: bool) -> PluginResult<()> {
        guarded(|| {
            let config = Config::load(Path::new(config_file))
                .map_err(|msg| GeyserPluginError::ConfigFileReadError { msg })?;
            let (runtime, feed) = Runtime::start(config).map_err(custom)?;
            self.running = Some(Running { feed, runtime });
            Ok(())
        })
    }

    /// The host is done with the plugin: the feed ends, and the runtime
    /// finishes what it was handed, so its journal is whole.
    fn on_unload(&mut self) {
        let Some(Running { feed, runtime }) = self.running.take() else {
            return;
        };
        drop(feed);
        runtime.join();
    }

    fn update_account(
        &self,
        account: ReplicaAccountInfoVersions,
        slot: u64,
        _is_startup: bool,
    ) -> PluginResult<()> {
        guarded(|| {
            let ReplicaAccountInfoVersions::V0_0_3(account) = account else {
                return Err(custom(
                    "the host's account notifications are older than 0.0.3",
                ));
            };
            let feed = self.feed()?;
            if !feed.watches(account.pubkey) {
                return Ok(());
            }
            feed.send(FeedEvent::Account(AccountWrite {
                slot,
                address: address(account.pubkey)?,
                owner: address(account.owner)?,
                lamports: account.lamports,
                data: account.data.to_vec(),
                write_version: account.write_version,
            }));
            Ok(())
        })
    }

    fn update_slot_status(
        &self,
        slot: u64,
        parent: Option<u64>,
        status: &HostSlotStatus,
    ) -> PluginResult<()> {
        guarded(|| {
            self.feed()?.send(FeedEvent::Slot {
                slot,
                parent,
                status: slot_status(status),
            });
            Ok(())
        })
    }

    fn notify_block_metadata(&self, block: ReplicaBlockInfoVersions) -> PluginResult<()> {
        guarded(|| {
            let ReplicaBlockInfoVersions::V0_0_4(block) = block else {
                return Err(custom(
                    "the host's block notifications are older than 0.0.4",
                ));
            };
            self.feed()?.send(FeedEvent::Block {
                slot: block.slot,
                blockhash: block.blockhash.to_string(),
                block_time_unix: block.block_time,
                block_height: block.block_height,
            });
            Ok(())
        })
    }

    fn account_data_notifications_enabled(&self) -> bool {
        true
    }

    /// The runtime reads no transactions yet: its own fills and the tape
    /// come with the desks.
    fn transaction_notifications_enabled(&self) -> bool {
        false
    }
}

/// Run a callback's body, and turn a panic into an error the host logs: it
/// must not cross into the host's thread.
fn guarded(body: impl FnOnce() -> PluginResult<()>) -> PluginResult<()> {
    catch_unwind(AssertUnwindSafe(body)).unwrap_or_else(|_| Err(custom("the plugin panicked")))
}

fn custom(reason: impl Into<String>) -> GeyserPluginError {
    GeyserPluginError::Custom(reason.into().into())
}

fn address(bytes: &[u8]) -> PluginResult<Address> {
    Address::try_from(bytes).map_err(|_| custom("an address that isn't 32 bytes"))
}

fn slot_status(status: &HostSlotStatus) -> SlotStatus {
    match status {
        HostSlotStatus::Processed => SlotStatus::Processed,
        HostSlotStatus::Rooted => SlotStatus::Rooted,
        HostSlotStatus::Confirmed => SlotStatus::Confirmed,
        HostSlotStatus::FirstShredReceived => SlotStatus::FirstShredReceived,
        HostSlotStatus::Completed => SlotStatus::Completed,
        HostSlotStatus::CreatedBank => SlotStatus::CreatedBank,
        HostSlotStatus::Dead(_) => SlotStatus::Dead,
    }
}

/// What a host calls to make the plugin.
///
/// # Safety
///
/// The host takes the pointer as a `Box<dyn GeyserPlugin>`: it must have
/// been built with this library's Rust toolchain and plugin interface.
#[unsafe(no_mangle)]
#[allow(improper_ctypes_definitions)]
pub unsafe extern "C" fn _create_plugin() -> *mut dyn GeyserPlugin {
    let plugin: Box<dyn GeyserPlugin> = Box::<MtxGeyser>::default();
    Box::into_raw(plugin)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plugin_that_isnt_loaded_refuses_its_callbacks() {
        let plugin = MtxGeyser::default();
        assert!(
            plugin
                .update_slot_status(1, None, &HostSlotStatus::Processed)
                .is_err()
        );
    }

    #[test]
    fn a_configuration_that_cant_be_read_fails_the_load() {
        let mut plugin = MtxGeyser::default();
        let loaded = plugin.on_load("/nonexistent/mtx-geyser.json", false);
        assert!(matches!(
            loaded,
            Err(GeyserPluginError::ConfigFileReadError { .. })
        ));
        // Unloading what never loaded is nothing.
        plugin.on_unload();
    }

    #[test]
    fn a_panic_in_a_callback_is_an_error_not_the_hosts_problem() {
        let caught = guarded(|| panic!("a bug"));
        assert!(caught.is_err());
    }

    #[test]
    fn every_status_the_host_names_has_ours() {
        let dead = HostSlotStatus::Dead("why".into());
        assert_eq!(slot_status(&dead), SlotStatus::Dead);
        assert_eq!(
            slot_status(&HostSlotStatus::FirstShredReceived),
            SlotStatus::FirstShredReceived
        );
        assert_eq!(slot_status(&HostSlotStatus::Rooted), SlotStatus::Rooted);
    }
}
