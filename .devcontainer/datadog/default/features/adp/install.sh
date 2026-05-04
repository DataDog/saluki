#!/usr/bin/env bash
set -xeuo pipefail

featureDir=$(cd "$(dirname "$0")"; pwd)

# Install Rust and cargo tools as the bits user so that ~/.cargo and ~/.rustup
# are owned by bits.
su -s /bin/bash bits --login "$featureDir/scripts/install_tools.sh"

# Copy lifecycle scripts into the image.
install -d /opt/doghome/devcontainer/features/adp/lifecycle
install -m 755 "$featureDir/lifecycle/postCreate.sh" /opt/doghome/devcontainer/features/adp/lifecycle/postCreate.sh

# Configure PATH for interactive shells.
# File name convention *-workspace-env.sh is important:
# /etc/zsh/zshenv sources these files.
cat > /etc/profile.d/10-adp-workspace-env.sh << 'EOF'
export PATH="/home/bits/.cargo/bin:$PATH"
EOF
