//! On-demand specialized worker VMs for orchestration.
//!
//! A role whose placement is `spawn` gets its own dedicated Firecracker microVM
//! for the duration of a run: the orchestrator picks a free network address,
//! derives a spec from a base agent (reusing its baked rootfs + harness auth),
//! materializes and launches the VM, installs the guest worker, runs the role's
//! steps on it, and tears it down at the end. This is what frees orchestration
//! from being limited to whatever agents happen to be running — it brings up as
//! many specialized workers as a run needs, up to the configured cap.
//!
//! This module owns the two pieces that are pure and therefore unit-testable:
//! choosing a non-colliding network triple, and deriving the role's spec from a
//! base. The live steps (create the host TAP, launch, install the worker, wait,
//! tear down) live behind `maturana-ops`, so front ends do not become VM
//! provisioning control planes.

use std::collections::HashSet;

use crate::spec::AgentSpec;
use crate::state::MaturanaHome;

/// The default subnet spawned worker VMs live on (the same /24 the standing
/// Firecracker agents use).
pub const DEFAULT_SUBNET: &str = "172.30.10";

/// A free Firecracker network address for a spawned worker VM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirecrackerNet {
    pub tap_name: String,
    pub host_ip: String,
    pub guest_ip: String,
    pub guest_mac: String,
    /// The /30 subnet base the host/guest pair sits in (e.g. host .13 → ".12/30"),
    /// passed to the TAP-setup script exactly like the standing agents' `cidr`.
    pub cidr: String,
}

/// Pick a non-colliding network triple in `subnet`. Each VM uses a host/guest
/// IP pair four apart, matching the standing agents' layout (host .1/.5/.9…,
/// guest = host+1), so a spawned worker never lands on an in-use pair. `used`
/// is the set of host octets already taken (from the standing agents and any
/// other live spawned workers). Returns `None` if the subnet is full.
pub fn allocate_net(subnet: &str, used_host_octets: &HashSet<u8>) -> Option<FirecrackerNet> {
    (0u8..62)
        .map(|k| 1 + k * 4)
        .find(|host| *host as u16 + 1 < 255 && !used_host_octets.contains(host))
        .map(|host| {
            let guest = host + 1;
            FirecrackerNet {
                tap_name: format!("tap-mat-orch-{host}"),
                host_ip: format!("{subnet}.{host}"),
                guest_ip: format!("{subnet}.{guest}"),
                // Locally-administered MAC; last octet tracks the guest IP so it
                // is unique per VM and easy to correlate.
                guest_mac: format!("06:00:AC:1E:0A:{guest:02X}"),
                // The host/guest pair sits in a /30 whose base is host-1
                // (.1/.2 → .0/30, .5/.6 → .4/30, .13/.14 → .12/30).
                cidr: format!("{subnet}.{}/30", host - 1),
            }
        })
}

/// Scan every materialized agent's spec for the Firecracker host-IP octets it
/// occupies, so [`allocate_net`] can avoid them. Best-effort: an agent whose
/// spec can't be read or isn't Firecracker is skipped.
pub fn used_host_octets(home: &MaturanaHome, subnet: &str) -> HashSet<u8> {
    let mut used = HashSet::new();
    let agents_dir = home.root().join("agents");
    let Ok(entries) = std::fs::read_dir(&agents_dir) else {
        return used;
    };
    for entry in entries.flatten() {
        let spec_path = entry.path().join("MATURANA.md");
        if !spec_path.exists() {
            continue;
        }
        if let Ok(spec) = AgentSpec::from_maturana_markdown(&spec_path) {
            if let Some(fc) = spec.vm.firecracker.as_ref() {
                if let Some(octet) = host_octet_in_subnet(&fc.host_ip, subnet) {
                    used.insert(octet);
                }
            }
        }
    }
    used
}

/// The host octet of `ip` if it is in `subnet` (e.g. "172.30.10.9" in
/// "172.30.10" → 9), else None.
fn host_octet_in_subnet(ip: &str, subnet: &str) -> Option<u8> {
    ip.strip_prefix(subnet)
        .and_then(|rest| rest.strip_prefix('.'))
        .and_then(|octet| octet.parse::<u8>().ok())
}

/// Derive a per-run, per-role worker spec from a base agent's spec: a unique id,
/// the allocated network, channels stripped (an ephemeral worker has no Telegram
/// bridge), and `start_on_boot` so its worker comes up ready to claim steps. The
/// base's harness, rootfs image, and harness auth are reused as-is, so the spawned
/// VM is the same kind of agent — just a fresh, isolated instance for this role.
pub fn derive_role_spec(base: &AgentSpec, new_id: &str, net: &FirecrackerNet) -> AgentSpec {
    let mut spec = base.clone();
    spec.identity.id = new_id.to_string();
    spec.identity.name = new_id.to_string();
    if let Some(fc) = spec.vm.firecracker.as_mut() {
        fc.tap_name = net.tap_name.clone();
        fc.host_ip = net.host_ip.clone();
        fc.guest_ip = net.guest_ip.clone();
        fc.guest_mac = net.guest_mac.clone();
    }
    // An ephemeral worker is reached only by the orchestrator over its own
    // session queue — it has no channels of its own and should be ready to work
    // as soon as it boots.
    spec.channels = Default::default();
    spec.schedules = Vec::new();
    spec.agent_run.start_on_boot = true;
    spec
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocate_skips_used_octets_and_pairs_host_with_guest() {
        // Standing agents occupy hosts 1, 5, 9 (guests 2, 6, 10).
        let used: HashSet<u8> = [1, 5, 9].into_iter().collect();
        let net = allocate_net(DEFAULT_SUBNET, &used).expect("an address should be free");
        // Next free host/guest pair is .13/.14.
        assert_eq!(net.host_ip, "172.30.10.13");
        assert_eq!(net.guest_ip, "172.30.10.14");
        assert_eq!(net.tap_name, "tap-mat-orch-13");
        assert_eq!(net.cidr, "172.30.10.12/30");
        assert!(net.guest_mac.starts_with("06:00:AC:1E:0A:"));
    }

    #[test]
    fn allocate_returns_none_when_full() {
        let used: HashSet<u8> = (0u8..62).map(|k| 1 + k * 4).collect();
        assert!(allocate_net(DEFAULT_SUBNET, &used).is_none());
    }

    #[test]
    fn host_octet_parses_only_within_subnet() {
        assert_eq!(host_octet_in_subnet("172.30.10.9", "172.30.10"), Some(9));
        assert_eq!(host_octet_in_subnet("10.0.0.9", "172.30.10"), None);
        assert_eq!(host_octet_in_subnet("172.30.10.x", "172.30.10"), None);
    }
}
