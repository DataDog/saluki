#!/usr/bin/env bash

# Make sure we have the AWS CLI installed and that our named profile has been specified.
command -v aws >/dev/null 2>&1 || { echo "AWS CLI not found. Please install it." >&2; exit 1; }
if [[ -z "${AWS_NAMED_PROFILE}" ]]; then
    echo "\$AWS_NAMED_PROFILE must be present"
    exit 1
fi

aws configure set aws_access_key_id $(aws ssm get-parameter --region us-east-1 --name ci.saluki.smp-bot-access-key-id --with-decryption --query "Parameter.Value" --out text) --profile ${AWS_NAMED_PROFILE}
aws configure set aws_secret_access_key $(aws ssm get-parameter --region us-east-1 --name ci.saluki.smp-bot-access-key --with-decryption --query "Parameter.Value" --out text) --profile ${AWS_NAMED_PROFILE}
aws configure set region us-west-2 --profile ${AWS_NAMED_PROFILE}
