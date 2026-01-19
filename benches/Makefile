# Path to dependencies
PATH_TO_USOCKETS=./uSockets
PARENT_DIR=./

# brew link --force openssl
# /opt/homebrew/opt/openssl@3/
OPENSSL_FLAGS=-L/opt/homebrew/opt/openssl@3/lib -I/opt/homebrew/opt/openssl@3/include

.PHONY: default setup setup-all check clean run

# Default target - build load_test only
default: setup
	clang -O3 -DLIBUS_USE_OPENSSL $(OPENSSL_FLAGS) -I$(PATH_TO_USOCKETS)/src load_test.c -c
	clang++ -O3 -DLIBUS_USE_OPENSSL $(OPENSSL_FLAGS) load_test.o $(PATH_TO_USOCKETS)/*.o -lssl -lcrypto -o load_test

# Setup target - downloads and builds uSockets for load_test
setup:
	@echo "Setting up benchmark dependencies..."
	@if [ ! -d "$(PATH_TO_USOCKETS)" ]; then \
		echo "Cloning uSockets..."; \
		git clone https://github.com/uNetworking/uSockets.git $(PATH_TO_USOCKETS); \
	else \
		echo "uSockets already exists"; \
	fi
	@echo "Building uSockets library..."
	@$(MAKE) -C $(PATH_TO_USOCKETS) WITH_OPENSSL=1 CFLAGS="$(OPENSSL_FLAGS)" CXXFLAGS="$(OPENSSL_FLAGS)"
	@echo "Setup complete!"

# Setup all WebSocket libraries for benchmarking
setup-all: setup
	@echo ""
	@echo "==> Setting up all WebSocket libraries for benchmarking..."
	@echo ""

	# 1. Build yawc echo_server
	@echo "==> Building yawc echo_server..."
	@cd .. && cargo build --release --example echo_server

	# 2. Clone and build fastwebsockets
	@echo ""
	@echo "==> Setting up fastwebsockets..."
	@if [ ! -d "$(PARENT_DIR)/fastwebsockets" ]; then \
		cd $(PARENT_DIR) && git clone https://github.com/denoland/fastwebsockets.git; \
	fi
	@cd $(PARENT_DIR)/fastwebsockets && cargo build --release --example echo_server --features upgrade

	# 3. Clone and build uWebSockets
	@echo ""
	@echo "==> Setting up uWebSockets..."
	@if [ ! -d "$(PARENT_DIR)/uWebSockets" ]; then \
		cd $(PARENT_DIR) && git clone --recursive https://github.com/uNetworking/uWebSockets.git; \
	fi
	@if [ ! -d "$(PARENT_DIR)/uWebSockets/uSockets" ]; then \
		cd $(PARENT_DIR)/uWebSockets && git submodule update --init --recursive; \
	fi
	@echo "Building uWebSockets EchoServer example..."
	@cd $(PARENT_DIR)/uWebSockets && WITH_OPENSSL=1 CFLAGS="$(OPENSSL_FLAGS)" CXXFLAGS="$(OPENSSL_FLAGS)" $(MAKE) examples

	# 4. Clone and build tokio-tungstenite
	@echo ""
	@echo "==> Setting up tokio-tungstenite..."
	@if [ ! -d "$(PARENT_DIR)/tokio-tungstenite" ]; then \
		cd $(PARENT_DIR) && git clone https://github.com/snapview/tokio-tungstenite.git; \
	fi
	@cd $(PARENT_DIR)/tokio-tungstenite && cargo build --release --example echo-server

	@echo ""
	@echo "==> All WebSocket libraries built successfully!"
	@echo ""

# Run benchmarks (requires Deno)
run: default
	@echo "Running benchmarks..."
	@if ! command -v deno &> /dev/null; then \
		echo "Error: Deno is required to run benchmarks"; \
		echo "Install it from: https://deno.land/"; \
		exit 1; \
	fi
	deno run --allow-all run.js

# Check target - verifies dependencies exist
check:
	@if [ ! -d "$(PATH_TO_USOCKETS)" ]; then \
		echo "Error: uSockets not found at $(PATH_TO_USOCKETS)"; \
		echo "Run 'make setup' to download and build dependencies"; \
		exit 1; \
	fi

clean:
	rm -f *.o load_test
