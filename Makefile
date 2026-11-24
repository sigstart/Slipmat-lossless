# Slipmat — build and install to a personal (per-user) prefix.
#
# No sudo: this is a one-user, one-machine app (see CLAUDE.md), so everything
# lands under ~/.local, which is already on PATH and XDG_DATA_DIRS. Override
# PREFIX for a system install (make PREFIX=/usr/local install, with sudo).
#
PREFIX  ?= $(HOME)/.local
BINDIR   = $(PREFIX)/bin
DATADIR  = $(PREFIX)/share
APPID    = dev.miguelrincon.Slipmat
# climat has an id of its own because it is a separate program with a separate
# entry — a terminal one, launched by the desktop into a terminal.
CLIMAT   = dev.miguelrincon.Climat

ICON_SIZES = 16 32 48 64 128 256 512

.PHONY: all build run test check gapless footprint install \
        dev-install fairplay-libs widevine fairplay-provider uninstall clean flatpak flatpak-bundle aur aur-publish
all: build

build:
	cargo build --release

run:
	cargo run

test:
	cargo test

# The bar from CLAUDE.md. --all-targets so tests are linted too.
check:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	cargo test
	# Keep the fast packaging checks local even though CI also builds bundles.
	# They catch stale sources and architecture drift before a seven-minute job.
	python3 packaging/flatpak/check-sources.py
	python3 packaging/check-architectures.py

# Watch the audio stream across a track boundary. Run it in one terminal and
# `RUST_LOG=slipmat=info cargo run` in another — the log says whether Rust
# drove the transition, this says whether the decoder stopped.
gapless:
	./scripts/gapless-check.sh

# What the app costs the machine: memory, CPU and disk. Needs a running
# instance for the first two; `--disk` alone needs nothing.
footprint:
	./scripts/footprint.sh

# A native `flatpak-builder` if there is one, otherwise the Flathub app. They
# are the same tool; the difference is that the flatpak'd one runs sandboxed,
# and on a CI runner that sandbox cannot see runtimes installed into the user
# installation — it fails with `Unable to find sdk org.gnome.Sdk version 49`
# twenty seconds after installing exactly that.
FLATPAK_BUILDER := $(shell command -v flatpak-builder >/dev/null 2>&1 \
	&& echo flatpak-builder || echo flatpak run org.flatpak.Builder)
FLATPAK_ARCH ?= $(shell uname -m)

flatpak:
	$(FLATPAK_BUILDER) --arch=$(FLATPAK_ARCH) --force-clean --user --install \
		--repo=flatpak-repo build-dir packaging/flatpak/dev.miguelrincon.Slipmat.yml

# `--runtime-repo` is the difference between a bundle that installs and one
# that stops because the matching GNOME runtime was not found. A .flatpak
# carries the *app* and never the runtime, so on a
# machine with no Flathub remote there is nothing for it to sit on and flatpak
# has no idea where to look. The URL is recorded inside the bundle, so
# installing it offers to add Flathub and pull the runtime itself.
#
# Found by installing on a clean Ubuntu VM, which is the only place it could
# have been found: every machine that has ever built this already had the
# runtime.
flatpak-bundle: flatpak
	flatpak build-bundle --arch=$(FLATPAK_ARCH) \
		--runtime-repo=https://dl.flathub.org/repo/flathub.flatpakrepo \
		flatpak-repo Slipmat-$(FLATPAK_ARCH).flatpak dev.miguelrincon.Slipmat master
	@echo "Slipmat-$(FLATPAK_ARCH).flatpak: copy it anywhere and install it with flatpak"

# Show what publishing to the AUR would do, without doing it. `make aur-publish`
# is the same thing with --push. Deliberately local rather than a CI job: the
# key that can publish under your name should not live in a repository secret.
aur:
	./scripts/aur-publish.sh slipmat-git

aur-publish:
	./scripts/aur-publish.sh slipmat-git --push

fairplay-libs:
	sh scripts/fairplay/prepare.sh

# Opt-in: Google's CDM, for songs Apple has no lossless master for.
widevine:
	sh scripts/fetch-widevine.sh

fairplay-provider: fairplay-libs
	sh scripts/fairplay/build-provider.sh

install: fairplay-libs build dev-install
	install -Dm755 target/release/slipmat $(BINDIR)/slipmat
	install -Dm755 target/release/slipmatd $(BINDIR)/slipmatd
	install -Dm755 target/release/climat $(BINDIR)/climat
	install -Dm644 packaging/systemd/slipmatd.service \
		$(DATADIR)/systemd/user/slipmatd.service
	@echo "Installed to $(PREFIX). Launch 'Slipmat' from the app grid, or run 'slipmat' — or 'climat' for the terminal."

# Everything except the binaries: the .desktop entry and the icons.
# Not a way to get a dev-mode icon — on Wayland only the fully installed app
# shows one.
dev-install:
	install -Dm644 data/$(APPID).desktop $(DATADIR)/applications/$(APPID).desktop
	install -Dm644 data/$(CLIMAT).desktop $(DATADIR)/applications/$(CLIMAT).desktop
	install -Dm644 data/icons/hicolor/scalable/apps/$(APPID).svg \
		$(DATADIR)/icons/hicolor/scalable/apps/$(APPID).svg
	install -Dm644 data/icons/hicolor/scalable/apps/$(CLIMAT).svg \
		$(DATADIR)/icons/hicolor/scalable/apps/$(CLIMAT).svg
	install -Dm644 data/icons/hicolor/symbolic/apps/$(APPID)-symbolic.svg \
		$(DATADIR)/icons/hicolor/symbolic/apps/$(APPID)-symbolic.svg
	@for icon in lossless lossy; do \
		install -Dm644 data/icons/hicolor/symbolic/apps/$(APPID)-$$icon-symbolic.svg \
			$(DATADIR)/icons/hicolor/symbolic/apps/$(APPID)-$$icon-symbolic.svg; \
	done
	@# Raster sizes, rendered from the same SVG the app installs so the two
	@# can never drift. GTK resolves the SVG on its own, but the shell, the
	@# notification daemon and anything reading the icon theme without an SVG
	@# loader all want PNGs — and this loop used to look for files that were
	@# never in the tree, so it silently installed none.
	@if command -v rsvg-convert >/dev/null 2>&1; then \
		for id in $(APPID) $(CLIMAT); do \
			for sz in $(ICON_SIZES); do \
				install -d $(DATADIR)/icons/hicolor/$${sz}x$${sz}/apps; \
				rsvg-convert -w $${sz} -h $${sz} \
					data/icons/hicolor/scalable/apps/$${id}.svg \
					-o $(DATADIR)/icons/hicolor/$${sz}x$${sz}/apps/$${id}.png; \
			done; \
		done; \
		echo "Rendered PNG icons: $(ICON_SIZES)"; \
	else \
		echo "rsvg-convert not found — installing the SVG only."; \
	fi
	@if [ -f $(DATADIR)/icons/hicolor/index.theme ]; then \
		touch $(DATADIR)/icons/hicolor; \
		gtk-update-icon-cache -q -t -f $(DATADIR)/icons/hicolor; \
	fi
	-update-desktop-database -q $(DATADIR)/applications

uninstall:
	rm -f $(BINDIR)/slipmat
	rm -f $(BINDIR)/slipmatd
	rm -f $(BINDIR)/climat
	rm -rf $(DATADIR)/slipmat
	rm -f $(DATADIR)/applications/$(APPID).desktop
	rm -f $(DATADIR)/applications/$(CLIMAT).desktop
	rm -f $(DATADIR)/icons/hicolor/scalable/apps/$(APPID).svg
	rm -f $(DATADIR)/icons/hicolor/scalable/apps/$(CLIMAT).svg
	rm -f $(DATADIR)/icons/hicolor/symbolic/apps/$(APPID)-symbolic.svg
	rm -f $(DATADIR)/icons/hicolor/symbolic/apps/$(APPID)-lossless-symbolic.svg
	rm -f $(DATADIR)/icons/hicolor/symbolic/apps/$(APPID)-lossy-symbolic.svg
	@for sz in $(ICON_SIZES); do \
		rm -f $(DATADIR)/icons/hicolor/$${sz}x$${sz}/apps/$(APPID).png; \
		rm -f $(DATADIR)/icons/hicolor/$${sz}x$${sz}/apps/$(CLIMAT).png; \
	done
	@if [ -f $(DATADIR)/icons/hicolor/index.theme ]; then \
		gtk-update-icon-cache -q -t -f $(DATADIR)/icons/hicolor; \
	fi
	-update-desktop-database -q $(DATADIR)/applications
	@echo "Uninstalled from $(PREFIX)."

clean:
	cargo clean
