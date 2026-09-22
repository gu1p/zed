.DEFAULT_GOAL := install

.PHONY: help install upgrade

help:
	@printf '%s\n' \
		'make install  Build and install this checkout locally (default; macOS or Linux).' \
		'make upgrade  Rebuild and replace the local installation with this checkout.' \
		'Install the build dependencies listed in docs/src/development/ first.'

install: export ZED_UPDATE_EXPLANATION = Update this local build by running make upgrade in its source checkout.
install:
	@case "$$(uname -s)" in \
		Darwin) ./script/bundle-mac -i ;; \
		Linux) ./script/install-linux ;; \
		*) printf '%s\n' 'Local installation supports macOS and Linux only.' >&2; exit 1 ;; \
	esac

upgrade: install
