# AUR development packaging

The `slipmat-git` base builds three x86-64 packages. Either frontend brings in
`slipmat-daemon-git`. Set up the native FairPlay provider separately using the
[development instructions](../../README.md#development).

`make aur` prepares and prints the package update. `make aur-publish` publishes
the split base from the maintainer's machine. Source releases can gain a stable
recipe once a native version is tagged.
