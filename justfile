# dagrun justfile

# build the project
build:
    cargo build --release

# run all unit tests
test:
    cargo test --lib

# run integration tests (excluding K8s tests)
test-integration:
    cargo test --test integration

# run all tests including K8s tests (requires kind cluster)
test-all: kind-ensure
    DAGRUN_K8S_TESTS=1 cargo test --test integration -- --include-ignored

# run only K8s tests
test-k8s: kind-ensure
    DAGRUN_K8S_TESTS=1 cargo test --test integration k8s -- --ignored

# create kind cluster for testing
kind-create:
    kind create cluster --name dagrun-test

# delete kind cluster
kind-delete:
    kind delete cluster --name dagrun-test

# ensure kind cluster exists
kind-ensure:
    #!/usr/bin/env bash
    if ! kind get clusters 2>/dev/null | grep -q dagrun-test; then
        echo "Creating kind cluster..."
        kind create cluster --name dagrun-test
    else
        echo "Kind cluster 'dagrun-test' already exists"
    fi

# run clippy
lint:
    cargo clippy -- -D warnings

# format code
fmt:
    cargo fmt

# check formatting
fmt-check:
    cargo fmt -- --check

# clean build artifacts
clean:
    cargo clean

# install locally
install:
    cargo install --path .

# run a quick smoke test
smoke-test: build
    ./target/release/dagrun -c examples/basic.dr run final

# validate all example configs
validate-examples:
    #!/usr/bin/env bash
    for f in examples/*.dr; do
        echo "Validating $f..."
        cargo run -- -c "$f" validate
    done

# generate graph PNG for complex example
graph-example:
    cargo run -- -c examples/complex-hybrid.dr graph -f png -o docs/complex-workflow.png

# watch and run tests on change
watch:
    cargo watch -x "test --lib"

# show help
help:
    @echo "Available recipes:"
    @just --list
