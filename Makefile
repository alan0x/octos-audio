.PHONY: check rust-check python-check web-check integration-check

PYTHON ?= $(if $(wildcard bridge/.venv/bin/python),.venv/bin/python,python3)

check: rust-check python-check web-check

rust-check:
	cd control-plane && cargo test --locked
	cd control-plane && cargo clippy --locked -- -D warnings

python-check:
	cd bridge && $(PYTHON) -m unittest discover -s tests -v

web-check:
	node --check control-plane/static/app.js
	node --test control-plane/tests/app.test.cjs
	bash -n bridge/start-real.sh
	bash -n bridge/start-ominix-asr.sh
	bash -n bridge/start-ominix-pool.sh
	bash -n bridge/start-ominix-tts.sh

integration-check:
	cd control-plane && cargo build --locked
	cd bridge && $(PYTHON) ../control-plane/tests/test_bridge_liveness.py ../control-plane/target/debug/agora-sensevoice-control-plane
