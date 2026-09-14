# Retained cloud lab recipes

These are the exact first-run Python/shell recipes, including failed versions.
They preserve the implementation and test setup behind the receipts. The supported
starting point for new repeatable clone measurements is the Rust `cloud-lab`
example and `docs/experiments/cloud-lab.md`.

Paths, snapshot identities and expected hashes here name the original disposable
lab. Review and adapt them before rerunning on a host you own. No script provisions
another cloud host automatically. Never include the referenced `*-private*` files
in evidence: those hold ephemeral SSH keys or signed storage URLs. None is checked
in here.

Version history matters: shared_store_v3 lacked a host sampler utility; v4 checks
it before launching. backings.py lost memory ownership; v2 fixes ownership; v3
also uses a copied disk baseline so integrity costs match. The UFFD v1 and v2
runs failed; v3 retains its page handler for the complete VM lifetime. The first
CI warm command polluted the provisioning acknowledgement; v2 redirects warm
output to a guest log. These failures are lessons, not successful measurements.

For the successful CI path, `prepare_ci.sh` builds the first Go toolstore.
The observed missing-`gh` fix adds `pkgs.gh` to that flake's Go package list,
then rebuilds into `lab/go-ci-v2`; `ci_in_rooms_v3.py` uses that store.
`prepare_ci_extra.py` expects this repaired flake and extends it with the offline
adapter-test tools. Its pinned Codex package uses `vendor/.../bin/codex`.
`ci_extra.py` first failed because the new image lacked a neighboring kernel;
copy the pinned kernel beside the new image before `ci_extra_v2.py` prepares it.
That version's matrix rejected uppercase case IDs without launching guests;
`ci_extra_v3.py` reuses the successful preparation with lowercase case IDs.

The plotting recipe takes `readout.json` and an output directory; it requires
Matplotlib. The evidence exporter uses an explicit exclusion list and scans for
private-key/signed-URL markers. Review that list for your own workload: it is not
a general-purpose secret detector and cannot make arbitrary captures publishable.
