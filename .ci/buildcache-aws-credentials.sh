#!/bin/sh

# Don't run the script if we're not in CI.
if [ "${CI}" != "true" ]; then
  echo "Not in CI. Skipping Buildcache configuration."
  exit 0
fi

# Create the configuration file path.
mkdir -p ~/.buildcache

# Get our credentials from IMDS and source them into the environment so we can write them out to the buildcache
# configuration file.
eval $(aws configure export-credentials --format env)

cat <<EOF > ~/.buildcache/config.json
{
  "s3_access": "${AWS_ACCESS_KEY_ID}",
  "s3_secret": "${AWS_SECRET_ACCESS_KEY}"
}
EOF
