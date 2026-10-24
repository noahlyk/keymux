# Local builds. The version comes from git (see build.rs):
#   tag v1.10.0 checked out      -> 1.10.0
#   8 commits past v1.10.0       -> 1.10.0+8.g0587d41
#   uncommitted changes          -> ...+dirty
# Release builds set KEYMUX_VERSION explicitly, so they keep the plain release version.

PREFIX ?= /usr/local
BIN := target/release/keymux

.PHONY: all build install uninstall version clean

all: build

build:
	cargo build --release --locked

install: build
	install -Dm755 $(BIN) $(DESTDIR)$(PREFIX)/bin/keymux
	@echo "installed keymux $$($(DESTDIR)$(PREFIX)/bin/keymux --version | cut -d' ' -f2) to $(DESTDIR)$(PREFIX)/bin/keymux"

uninstall:
	rm -f $(DESTDIR)$(PREFIX)/bin/keymux

version: build
	@$(BIN) --version

clean:
	cargo clean
