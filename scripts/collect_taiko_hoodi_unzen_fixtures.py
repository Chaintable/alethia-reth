#!/usr/bin/env python3
"""Collect and verify pinned Taiko Hoodi Unzen-boundary replay inputs.

Running the script without arguments performs offline verification. Network
access is only used by the explicit ``fetch`` command.
"""

from __future__ import annotations

import argparse
from pathlib import Path

import collect_taiko_mainnet_fixtures as common


CHAIN_ID = 167013
DEFAULT_RPC_URL = "https://rpc.hoodi.taiko.xyz"
DEFAULT_OUTPUT_ROOT = (
    common.REPOSITORY_ROOT
    / "crates"
    / "rpc"
    / "tests"
    / "fixtures"
    / "taiko-hoodi"
)
EMPTY_WITHDRAWALS_ROOT = (
    "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421"
)
EMPTY_REQUESTS_HASH = (
    "0xe3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
)
ANCHOR_PROXY_SLOTS = (
    common.storage_key("0xc9"),
    common.storage_key("0x100"),
    common.storage_key("0x101"),
    "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc",
)
SYSTEM_ACCOUNTS = (
    {
        "role": "eip2935_system_caller_absence",
        "address": "0xfffffffffffffffffffffffffffffffffffffffe",
        "storage_keys": (),
    },
    {
        "role": "eip2935_history_storage_absence",
        "address": "0x0000f90827f1c53a10cb7a02335b175320002935",
        "storage_keys": (),
    },
    {
        "role": "eip4788_beacon_roots_absence",
        "address": "0x000f3df6d732807ef1319fb7b8bb8522d0beac02",
        "storage_keys": (),
    },
)
T_PLUS_ONE_SENDERS = (
    "0x1176f5156d4b8962448da1b99322cda0092cc759",
    "0xa4d8a869f46f1d336f124d44b6be1ac315fb2667",
    "0x2bea3824197bdfb19e7eb7aad05626dc271bb181",
    "0xd91d90de936de28a1e0c83d3be31862a29f1de9c",
    "0x64e8504fbe42f2d6edb72c8929fba24dbb5dcb85",
    "0x7301cc85b8c93f2a52675025810f9e41fc80399b",
    "0x695689dc13fc220095d856bda41e5b4899ac3da7",
    "0x314bd0214210c7fe9fbceafef4c3e6e664bb1d31",
    "0x89682d58c16f95a682f00643c3599d1565cc73d1",
    "0x2532efe0fbdb127794b94b70bf63732556f0d094",
)


def replay_accounts(mapping_slot: str) -> tuple[dict[str, object], ...]:
    """Return the four shared Anchor accounts for one block."""
    return (
        {
            "role": "golden_touch",
            "address": "0x0000777735367b36bc9b61c50022d9d0700db4ec",
            "storage_keys": (),
        },
        {
            "role": "anchor_treasury_proxy",
            "address": "0x1670130000000000000000000000000000010001",
            "storage_keys": ANCHOR_PROXY_SLOTS + (mapping_slot,),
        },
        {
            "role": "anchor_implementation",
            "address": "0x70a65ddf64960b9901df488825c1cbfbc9ae9685",
            "storage_keys": (),
        },
        {
            "role": "beneficiary",
            "address": "0x75141cd01f50a17a915d59d245ae6b2c947d37d9",
            "storage_keys": (),
        },
    )


def sender_accounts() -> tuple[dict[str, object], ...]:
    """Return the ten T+1 self-transfer senders."""
    return tuple(
        {
            "role": f"self_transfer_sender_{index}",
            "address": address,
            "storage_keys": (),
        }
        for index, address in enumerate(T_PLUS_ONE_SENDERS, start=1)
    )


PRE_UNZEN_MEMBERS = {
    "withdrawals": [],
    "withdrawalsRoot": EMPTY_WITHDRAWALS_ROOT,
    "difficulty": "0x0",
    "parentBeaconBlockRoot": common.ABSENT,
    "blobGasUsed": common.ABSENT,
    "excessBlobGas": common.ABSENT,
    "requestsHash": common.ABSENT,
    "blockAccessListHash": common.ABSENT,
    "slotNumber": common.ABSENT,
}
UNZEN_MEMBERS = {
    "withdrawals": [],
    "withdrawalsRoot": EMPTY_WITHDRAWALS_ROOT,
    "parentBeaconBlockRoot": "0x" + "00" * 32,
    "blobGasUsed": "0x0",
    "excessBlobGas": "0x0",
    "requestsHash": EMPTY_REQUESTS_HASH,
    "blockAccessListHash": common.ABSENT,
    "slotNumber": common.ABSENT,
}

FIXTURES = (
    {
        "chain_id": CHAIN_ID,
        "height": 11981107,
        "block_hash": "0x8604b9c28098ecd4c5071f2ae3ade535cb9f8938415aac902de8ce04abd6eb6f",
        "parent_hash": "0x0e32ebbb38163f6713f096608bb51add1fa8cc715a5bb4d6eef5da9e805de32d",
        "parent_state_root": "0xd02d907e5855ef8b301f3c2e854ce4e9a78e8a2acb175dbd95c5a4a51c3ead09",
        "state_root": "0xf9ee1a55b3873382836cb96eb0b38c059012d83083a3056e93b29def19211c0b",
        "transactions_root": "0x0c4c1c12c2bd59760b83902b95dcd80a3bca8a1744b82c90e92825f7d02cd552",
        "receipts_root": "0x3a335a27b689722f452dcea59a6743ba2b00c6386f62ae2dafe2a8c17e7520d7",
        "gas_used": "0x1b5a9",
        "timestamp": "0x6a33ebcf",
        "extra_data": "0x4b00000000a753",
        "beneficiary": "0x75141cd01f50a17a915d59d245ae6b2c947d37d9",
        "transaction_hashes": (
            "0x8fc5ba8adcd01dc9bd561e860d60c9345554a8107dcfeb73ac5079c2c8a33c63",
        ),
        "accounts": replay_accounts(
            "0xff1a59b1e9fe74bcf782f0f0a0250746dfc65a67d5c8ecbc810d96f3d807cf5a"
        ),
        "nonempty_code_count": 2,
        "header_members": PRE_UNZEN_MEMBERS,
        "block_hash_batch_size": 8,
        "block_hash_batch_delay": 0.5,
        "proof_batch_size": 8,
        "proof_batch_delay": 0.5,
    },
    {
        "chain_id": CHAIN_ID,
        "height": 11981108,
        "block_hash": "0x9b33c3e26bad5bbad176a1c7e8d51fc4a2d91703c851305e141e45f0e28a3993",
        "parent_hash": "0x8604b9c28098ecd4c5071f2ae3ade535cb9f8938415aac902de8ce04abd6eb6f",
        "parent_state_root": "0xf9ee1a55b3873382836cb96eb0b38c059012d83083a3056e93b29def19211c0b",
        "state_root": "0x9855686e6f701b4fd82dc2e6fecdea4a7aa2b677cf4eb1a826b7c7b677b73665",
        "transactions_root": "0x348f0e1028383e7c07c27eb3b5fddebebd879517a1a373c2f3a38a090df4ccd3",
        "receipts_root": "0xe3136ab228c695011325370061c94c9ed39b54b535cf9acf33b71c18df4f285e",
        "gas_used": "0x1b5a9",
        "timestamp": "0x6a33ebd0",
        "extra_data": "0x4b00000000a753",
        "beneficiary": "0x75141cd01f50a17a915d59d245ae6b2c947d37d9",
        "transaction_hashes": (
            "0x9460d02fd78de5e94de8687fb659ff774304d73c5c8f053237706dd058389677",
        ),
        "accounts": replay_accounts(
            "0xc54edfd5cd791b250b42746695907826af008eef76ae46335bfeff18557ce24f"
        )
        + SYSTEM_ACCOUNTS,
        "nonempty_code_count": 2,
        "header_members": {
            **UNZEN_MEMBERS,
            "difficulty": "0x11b626",
        },
        "block_hash_batch_size": 8,
        "block_hash_batch_delay": 0.5,
        "proof_batch_size": 8,
        "proof_batch_delay": 0.5,
    },
    {
        "chain_id": CHAIN_ID,
        "height": 11981109,
        "block_hash": "0xc737239de9884521bf78c2c4bddb110ed871bf67f3e356de102a3b47eb741c6a",
        "parent_hash": "0x9b33c3e26bad5bbad176a1c7e8d51fc4a2d91703c851305e141e45f0e28a3993",
        "parent_state_root": "0x9855686e6f701b4fd82dc2e6fecdea4a7aa2b677cf4eb1a826b7c7b677b73665",
        "state_root": "0xc3031da789d2a3da71e2da61798d23302ebd60635a7b1287e99062fcf7f2a863",
        "transactions_root": "0xbd407129211e9114ce1705b06b3b8eb0bb0dedb17d1025bbc06e3ab4c295ea7f",
        "receipts_root": "0x83108f116edb3677d104502fb1301387cef1791027f7c8b3fc17a7b87ae52f1f",
        "gas_used": "0x4e9f9",
        "timestamp": "0x6a33ebd1",
        "extra_data": "0x4b00000000a753",
        "beneficiary": "0x75141cd01f50a17a915d59d245ae6b2c947d37d9",
        "transaction_hashes": (
            "0xbf5aaa92601af25a3a91f4dda0c1dcd8e0f0910bf267f7c8f2b22d2d95129415",
            "0xba86ea4134ac6151d92c491a00e804d03b4c9c09578f3f176f11fbf62dca9703",
            "0x1539343a3e541e59b096c650dcd544e0ecd273d1ebd0f46cad0160327d2fc9fd",
            "0xda3fd10ac389cd442527fdb95bbd111124300d267b295000d4f0206e11061a44",
            "0xfd2f10b23bdadb21daf719f8223b3537803b08630de26d370173ef0c07c977ff",
            "0x5d84a1ac318a20eb502ade5c027402c81863bedd169e6f3ec922bc4679f42212",
            "0xc39652ecafdae964bc9025eb91ad0af50272dc0466c2ab3dcbe49f7bf5d9bf20",
            "0xe089b1e5752da3470d2b334b33c280ab1cc2cf2aacbf4d12f4353f66f9cb0862",
            "0x5f876d370309990cc4c557e5c8782e48f9c36578f85aeaa307f2dc6ebebf200f",
            "0x5cde9c56da9193a634d5ac19c49f37be409ceb147dd9045599fbab5f354db1ec",
            "0x45ab688b0719a04be2d251e433016c505552628c04c191783cebb6756ab66dc6",
        ),
        "accounts": replay_accounts(
            "0xbddd98d0f93b8cd6070e56728a7655fda5f58f6d6e4cfb486c50ac04846b4c17"
        )
        + SYSTEM_ACCOUNTS
        + sender_accounts(),
        "nonempty_code_count": 2,
        "header_members": {
            **UNZEN_MEMBERS,
            "difficulty": "0x36ca56",
        },
        "block_hash_batch_size": 8,
        "block_hash_batch_delay": 0.5,
        "proof_batch_size": 8,
        "proof_batch_delay": 0.5,
    },
)


def parse_args() -> argparse.Namespace:
    """Parse the explicit fetch or default offline verify command."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "command",
        choices=("fetch", "verify"),
        default="verify",
        nargs="?",
        help="fetch uses the network; verify is offline and is the default",
    )
    parser.add_argument(
        "--output-root",
        type=Path,
        default=DEFAULT_OUTPUT_ROOT,
        help=f"fixture directory (default: {DEFAULT_OUTPUT_ROOT})",
    )
    parser.add_argument(
        "--rpc-url",
        default=DEFAULT_RPC_URL,
        help=f"RPC URL used only by fetch (default: {DEFAULT_RPC_URL})",
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=120.0,
        help="per-request timeout in seconds for fetch (default: 120)",
    )
    parser.add_argument(
        "--retries",
        type=int,
        default=8,
        help="transport/rate-limit attempts for fetch (default: 8)",
    )
    args = parser.parse_args()
    common.require(args.timeout > 0, "--timeout must be positive")
    common.require(args.retries > 0, "--retries must be positive")
    return args


def main() -> int:
    """Run collection or offline verification."""
    try:
        args = parse_args()
        output_root = args.output_root.resolve()
        if args.command == "fetch":
            common.fetch_all(
                output_root,
                args.rpc_url,
                args.timeout,
                args.retries,
                fixtures=FIXTURES,
                temporary_prefix=".taiko-hoodi-unzen-fixtures-",
                collector_path=Path(__file__).resolve(),
            )
        else:
            common.verify_all(output_root, fixtures=FIXTURES)
        return 0
    except common.FixtureError as error:
        print(f"FAIL: {error}")
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
