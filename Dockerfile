FROM ubuntu:22.04

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Copy the built binary
COPY target/release/testapp /usr/local/bin/testapp

# Set entrypoint
ENTRYPOINT ["/usr/local/bin/testapp"]