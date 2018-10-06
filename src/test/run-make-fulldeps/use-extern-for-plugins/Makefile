-include ../tools.mk

# ignore-freebsd
# ignore-openbsd
# ignore-sunos

TARGET := $(shell $(RUSTC) -vV | grep 'host:' | sed 's/host: //')

all:
	$(RUSTC) foo.rs -C extra-filename=-host
	$(RUSTC) bar.rs -C extra-filename=-targ --target $(TARGET)
	$(RUSTC) baz.rs --extern a=$(TMPDIR)/liba-targ.rlib --target $(TARGET)
