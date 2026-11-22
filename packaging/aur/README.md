# AUR development packaging

The `slipmat-git` base builds three x86-64 packages. Either frontend brings in
`slipmat-daemon-git`. For local library staging and provider requirements,
see the [FairPlay guide](../../vendor/fairplay/README.md).

`make aur` prepares and prints the package update. `make aur-publish` publishes
the split base from the maintainer's machine. Source releases can gain a stable
recipe once a native version is tagged.
