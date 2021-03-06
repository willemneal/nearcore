#!/bin/bash
set -e

export NEAR_HOME=/srv/near

if [[ -z {INIT} ]]
then
    near --home=${NEAR_HOME} init --chain-id=${CHAIN_ID} --account-id=${ACCOUNT_ID}
fi

if [[ -z {NODE_KEY} ]]
then

    cat << EOF > ${NEAR_HOME}/node_key.json
{"account_id": "", "public_key": "", "secret_key": "$NODE_KEY"}
EOF

fi

echo "Bootnodes: ${BOOT_NODES}"

near --home=${NEAR_HOME} run --boot-nodes=${BOOT_NODES}

