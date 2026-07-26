#!/usr/bin/env python3
"""Collect and verify pinned Taiko mainnet replay inputs.

Running the script without arguments performs offline verification. Network
access is only used by explicit ``fetch`` commands.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import shutil
import tempfile
import time
import urllib.error
import urllib.request
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


CHAIN_ID = 167000
DEFAULT_RPC_URL = "https://rpc.mainnet.taiko.xyz"
REPOSITORY_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_OUTPUT_ROOT = (
    REPOSITORY_ROOT / "crates" / "rpc" / "tests" / "fixtures" / "taiko-mainnet"
)
PAYLOAD_FILES = (
    "block.json",
    "parent.json",
    "block-hashes.json",
    "raw-transactions.json",
    "receipts.json",
    "state-proofs.json",
    "codes.json",
)
KECCAK_EMPTY = "0xc5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470"
ZERO_HASH = "0x" + "00" * 32
ABSENT = object()
MASK_64 = (1 << 64) - 1
KECCAK_ROUND_CONSTANTS = (
    0x0000000000000001,
    0x0000000000008082,
    0x800000000000808A,
    0x8000000080008000,
    0x000000000000808B,
    0x0000000080000001,
    0x8000000080008081,
    0x8000000000008009,
    0x000000000000008A,
    0x0000000000000088,
    0x0000000080008009,
    0x000000008000000A,
    0x000000008000808B,
    0x800000000000008B,
    0x8000000000008089,
    0x8000000000008003,
    0x8000000000008002,
    0x8000000000000080,
    0x000000000000800A,
    0x800000008000000A,
    0x8000000080008081,
    0x8000000000008080,
    0x0000000080000001,
    0x8000000080008008,
)
KECCAK_ROTATIONS = (
    0,
    1,
    62,
    28,
    27,
    36,
    44,
    6,
    55,
    20,
    3,
    10,
    43,
    25,
    39,
    41,
    45,
    15,
    21,
    8,
    18,
    2,
    61,
    56,
    14,
)


def storage_key(value: str) -> str:
    """Return a canonical 32-byte storage key."""
    return f"0x{int(value, 16):064x}"


ANCHOR_PROXY_SLOTS = (
    storage_key("0xc9"),
    storage_key("0x100"),
    storage_key("0x101"),
    "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc",
)
TOKEN_PROXY_SLOTS = (
    storage_key("0x1"),
    "0x10d6a54a4754c8869d6886b5f5d7fbfa5b4522237ea5c60d11bc4e7a1ff9390b",
    "0x7050c9e0f4ca769c69bd3a8ef740bc37934f8e2c036e5a723fd8ee048ed3f8c3",
    "0x78d2b859763559879c4106865261ca8a485384b08244c04d8bb4160381e2705a",
)
COMMON_ACCOUNTS = (
    {
        "role": "golden_touch",
        "address": "0x0000777735367b36bc9b61c50022d9d0700db4ec",
        "storage_keys": (),
    },
    {
        "role": "anchor_proxy",
        "address": "0x1670000000000000000000000000000000010001",
        "storage_keys": ANCHOR_PROXY_SLOTS,
    },
    {
        "role": "beneficiary",
        "address": "0x5f62d006c10c009ff50c878cd6157ac861c99990",
        "storage_keys": (),
    },
    {
        "role": "anchor_implementation",
        "address": "0x7e83af941fdcf90eb44ed7dc8754a201b156e0ba",
        "storage_keys": (),
    },
)


def accounts_with_anchor_slots(*storage_keys: str) -> tuple[dict[str, Any], ...]:
    """Return common accounts with fixture-specific Anchor mapping slots."""
    return tuple(
        {
            **account,
            "storage_keys": account["storage_keys"] + storage_keys
            if account["role"] == "anchor_proxy"
            else account["storage_keys"],
        }
        for account in COMMON_ACCOUNTS
    )


FIXTURES = (
    {
        "height": 9069694,
        "block_hash": "0x463923d37a86715eb25d60c4e19ffd070e848e4f2dbe85d056affd5dda0f7697",
        "parent_hash": "0x06c4258ce90ec6336450d49e58a4d86901ac734f09daa8c4fd5fda95df112f41",
        "parent_state_root": "0xbd9cc9d2122e65eb68d09f3ff1609f6183838774812c10a5387eefea426344d2",
        "state_root": "0x16183012793b725168d06c970c291d6f81a082a3ed304c1503651da554ad7282",
        "transactions_root": "0x0516467c36723e0c9853974cddbe66b4a1350aa72bb73576e80e08afd4260e7a",
        "receipts_root": "0xdf859ba99d62a53cc16594ea95f1cca7a79532a2bda91c273860ee7088a3fa09",
        "gas_used": "0x28d91",
        "timestamp": "0x6a60aacf",
        "extra_data": "0x4b000000005a84",
        "beneficiary": "0x5f62d006c10c009ff50c878cd6157ac861c99990",
        "transaction_hashes": (
            "0x4a3e68839652184c505ec0ec12c78560bc16e225fb62928f412212f90b3b658e",
            "0x3cf1fe00ef1d400f1a9e30fe9f432000b2472f4f3e76f9166a50f563ca34a786",
        ),
        "accounts": accounts_with_anchor_slots(
            "0x7e8b43f57672109ae520c8900d03d91acdfe1106c84be426229f899409cf0680"
        )
        + (
            {
                "role": "normal_transaction_sender",
                "address": "0x962f6a4676565d4434090055f9f49549ab7b508f",
                "storage_keys": (),
            },
            {
                "role": "token_proxy",
                "address": "0x07d83526730c7438048d55a4fc0b850e2aab6f0b",
                "storage_keys": TOKEN_PROXY_SLOTS,
            },
            {
                "role": "token_implementation",
                "address": "0x996a7a32c387fd83e127a358fbc192e110459f2d",
                "storage_keys": (),
            },
        ),
        "nonempty_code_count": 4,
    },
    {
        "height": 9069705,
        "block_hash": "0x88391dc1a9a682541c3a05fb99e2f057993ba3ead040717c76ef8ae6eea3a9f4",
        "parent_hash": "0xe89bfa9a0dd965c682d607720c0f23663e5227c26a567e514b8d146b62072f3c",
        "parent_state_root": "0x42ca05e380d9dfdc5e30a97abeb370384ec6a1304920a561e8872250f390b89f",
        "state_root": "0xc8ddfefbacfe39f2fadcda29ffe27c4d4f789713feaf46e523df2fb432a7f3b8",
        "transactions_root": "0x98f1299d0fc224664d9285b4272c0ee9a21bc03166ce252b75adc6d7cac0a918",
        "receipts_root": "0x38a5f44d33d58dac168383d4c0c88cd8f8864ed5c0548ffb35365fa4083d57e8",
        "gas_used": "0x1b5c4",
        "timestamp": "0x6a60aae5",
        "extra_data": "0x4b000000005a84",
        "beneficiary": "0x5f62d006c10c009ff50c878cd6157ac861c99990",
        "transaction_hashes": (
            "0xecd9dc946e7cd898ec21b5312977726386d8c1b881cb44dabadf2c10732e9e0d",
        ),
        "accounts": accounts_with_anchor_slots(
            "0x04a6e7887a8b2e638810b88b876207ef0b5ce6dd79e6f76629f7a29ca2341f91"
        ),
        "nonempty_code_count": 2,
    },
)


class FixtureError(RuntimeError):
    """Report an invalid RPC response or frozen fixture."""


def require(condition: bool, message: str) -> None:
    """Raise a fixture error when an invariant is false."""
    if not condition:
        raise FixtureError(message)


def fixture_chain_id(spec: dict[str, Any]) -> int:
    """Return the fixture chain id, preserving mainnet as the historical default."""
    chain_id = spec.get("chain_id", CHAIN_ID)
    require(type(chain_id) is int and chain_id > 0, "fixture chain id must be positive")
    return chain_id


def rotate_left(value: int, shift: int) -> int:
    """Rotate one 64-bit Keccak lane."""
    if shift == 0:
        return value
    return ((value << shift) | (value >> (64 - shift))) & MASK_64


def keccak_permute(state: list[int]) -> None:
    """Apply Keccak-f[1600] to a 25-lane state."""
    for round_constant in KECCAK_ROUND_CONSTANTS:
        columns = [
            state[x] ^ state[x + 5] ^ state[x + 10] ^ state[x + 15] ^ state[x + 20]
            for x in range(5)
        ]
        deltas = [
            columns[(x - 1) % 5] ^ rotate_left(columns[(x + 1) % 5], 1)
            for x in range(5)
        ]
        for y in range(5):
            for x in range(5):
                state[x + 5 * y] ^= deltas[x]

        rotated = [0] * 25
        for y in range(5):
            for x in range(5):
                target = y + 5 * ((2 * x + 3 * y) % 5)
                rotated[target] = rotate_left(
                    state[x + 5 * y], KECCAK_ROTATIONS[x + 5 * y]
                )

        for y in range(5):
            row = rotated[5 * y : 5 * y + 5]
            for x in range(5):
                state[x + 5 * y] = (
                    row[x] ^ ((~row[(x + 1) % 5]) & row[(x + 2) % 5])
                ) & MASK_64
        state[0] ^= round_constant


def keccak256(data: bytes) -> bytes:
    """Compute legacy Keccak-256 without third-party Python packages."""
    rate = 136
    padded = bytearray(data)
    padded.append(0x01)
    padded.extend(b"\x00" * ((rate - 1 - len(padded) % rate) % rate))
    padded.append(0x80)

    state = [0] * 25
    for offset in range(0, len(padded), rate):
        block = padded[offset : offset + rate]
        for index, byte in enumerate(block):
            state[index // 8] ^= byte << (8 * (index % 8))
        keccak_permute(state)

    output = bytearray()
    for lane in state:
        output.extend(lane.to_bytes(8, "little"))
        if len(output) >= 32:
            return bytes(output[:32])
    raise AssertionError("Keccak state did not produce 32 bytes")


def decode_data(value: Any, label: str) -> bytes:
    """Decode an even-length 0x-prefixed data value."""
    require(isinstance(value, str) and value.startswith("0x"), f"{label}: expected hex data")
    encoded = value[2:]
    require(len(encoded) % 2 == 0, f"{label}: odd-length hex data")
    try:
        return bytes.fromhex(encoded)
    except ValueError as error:
        raise FixtureError(f"{label}: invalid hex data") from error


def quantity(value: Any, label: str) -> int:
    """Decode a JSON-RPC quantity."""
    require(isinstance(value, str) and value.startswith("0x"), f"{label}: expected quantity")
    require(value == "0x0" or not value.startswith("0x0"), f"{label}: non-canonical quantity")
    try:
        return int(value, 16)
    except ValueError as error:
        raise FixtureError(f"{label}: invalid quantity") from error


def utc_now() -> str:
    """Return a second-precision UTC timestamp."""
    return datetime.now(timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z")


def sha256_file(path: Path) -> str:
    """Hash one file."""
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def read_json(path: Path) -> Any:
    """Read one UTF-8 JSON file."""
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise FixtureError(f"{path}: cannot read JSON: {error}") from error


def write_json(path: Path, value: Any) -> None:
    """Write stable JSON without removing null or empty members."""
    path.write_text(
        json.dumps(value, indent=2, ensure_ascii=False) + "\n",
        encoding="utf-8",
    )


class RpcClient:
    """Minimal retrying JSON-RPC batch client."""

    def __init__(self, url: str, timeout: float, retries: int) -> None:
        self.url = url
        self.timeout = timeout
        self.retries = retries
        self.next_id = 1

    def batch(self, calls: list[tuple[str, list[Any]]]) -> list[Any]:
        """Execute a JSON-RPC batch and return results in request order."""
        require(bool(calls), "RPC batch cannot be empty")
        requests = []
        request_ids = []
        for method, params in calls:
            request_id = self.next_id
            self.next_id += 1
            request_ids.append(request_id)
            requests.append(
                {
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "method": method,
                    "params": params,
                }
            )
        encoded = json.dumps(requests, separators=(",", ":")).encode("utf-8")

        for attempt in range(self.retries):
            request = urllib.request.Request(
                self.url,
                data=encoded,
                headers={
                    "content-type": "application/json",
                    "user-agent": "alethia-reth-taiko-fixture-collector/1",
                },
                method="POST",
            )
            try:
                with urllib.request.urlopen(request, timeout=self.timeout) as response:
                    decoded = json.loads(response.read())
            except (TimeoutError, urllib.error.URLError, json.JSONDecodeError) as error:
                if attempt + 1 == self.retries:
                    raise FixtureError(
                        f"RPC transport failed after {self.retries} attempts: {error}"
                    ) from error
                time.sleep(min(2**attempt, 8))
                continue

            require(isinstance(decoded, list), "RPC batch response is not an array")
            by_id: dict[int, dict[str, Any]] = {}
            for item in decoded:
                require(isinstance(item, dict), "RPC batch item is not an object")
                response_id = item.get("id")
                require(
                    isinstance(response_id, int) and response_id not in by_id,
                    "RPC batch has an invalid or duplicate id",
                )
                by_id[response_id] = item
            require(
                set(by_id) == set(request_ids),
                "RPC batch response id set differs from request",
            )

            errors = [
                by_id[request_id]["error"]
                for request_id in request_ids
                if "error" in by_id[request_id]
            ]
            rate_limited = any(
                isinstance(error, dict) and error.get("code") == -32005 for error in errors
            )
            if rate_limited and attempt + 1 < self.retries:
                time.sleep(min(2**attempt, 8))
                continue
            require(not errors, f"RPC batch failed: {errors}")

            results = []
            for request_id in request_ids:
                item = by_id[request_id]
                require("result" in item, f"RPC request {request_id} omitted result")
                results.append(item["result"])
            return results

        raise FixtureError("RPC batch exhausted retries")


def expected_storage_count(spec: dict[str, Any]) -> int:
    """Return the number of requested storage proofs."""
    return sum(len(account["storage_keys"]) for account in spec["accounts"])


def validate_header(header: Any, spec: dict[str, Any], parent: bool) -> None:
    """Validate a fetched child or parent header against its fixed anchor."""
    label = "parent" if parent else "block"
    require(isinstance(header, dict), f"{label}: expected object")
    if parent:
        require(header.get("hash") == spec["parent_hash"], "parent: hash mismatch")
        require(
            quantity(header.get("number"), "parent.number") == spec["height"] - 1,
            "parent: height mismatch",
        )
        require(
            header.get("stateRoot") == spec["parent_state_root"],
            "parent: state root mismatch",
        )
        return

    expected_fields = {
        "hash": spec["block_hash"],
        "parentHash": spec["parent_hash"],
        "stateRoot": spec["state_root"],
        "transactionsRoot": spec["transactions_root"],
        "receiptsRoot": spec["receipts_root"],
        "gasUsed": spec["gas_used"],
        "timestamp": spec["timestamp"],
        "extraData": spec["extra_data"],
        "miner": spec["beneficiary"],
    }
    for field, expected in expected_fields.items():
        require(header.get(field) == expected, f"block.{field}: expected {expected}")
    require(
        quantity(header.get("number"), "block.number") == spec["height"],
        "block: height mismatch",
    )
    expected_members = spec.get(
        "header_members",
        {
            "withdrawals": [],
            "requestsHash": ABSENT,
        },
    )
    require(isinstance(expected_members, dict), "block: invalid expected header member set")
    for field, expected in expected_members.items():
        if expected is ABSENT:
            require(field not in header, f"block.{field}: expected field to be absent")
        else:
            require(field in header, f"block.{field}: expected field to be present")
            require(header[field] == expected, f"block.{field}: expected {expected}")


def validate_block_hashes(spec: dict[str, Any], block_hashes: Any) -> None:
    """Validate the complete EVM BLOCKHASH lookup window."""
    require(isinstance(block_hashes, list), "block-hashes: expected array")
    require(len(block_hashes) == 256, "block-hashes: expected exactly 256 entries")
    expected_numbers = list(range(spec["height"] - 256, spec["height"]))
    actual_numbers = []
    actual_hashes = []
    for index, entry in enumerate(block_hashes):
        require(isinstance(entry, dict), f"block-hashes[{index}]: expected object")
        require(
            set(entry) == {"number", "hash"},
            f"block-hashes[{index}]: unexpected field set",
        )
        number = entry["number"]
        require(type(number) is int, f"block-hashes[{index}].number: expected integer")
        block_hash = entry["hash"]
        decoded_hash = decode_data(block_hash, f"block-hashes[{index}].hash")
        require(len(decoded_hash) == 32, f"block-hashes[{index}].hash: expected 32 bytes")
        require(block_hash == block_hash.lower(), f"block-hashes[{index}].hash: expected lowercase")
        actual_numbers.append(number)
        actual_hashes.append(block_hash)
    require(actual_numbers == expected_numbers, "block-hashes: range is missing or discontinuous")
    require(len(set(actual_numbers)) == 256, "block-hashes: duplicate height")
    require(len(set(actual_hashes)) == 256, "block-hashes: duplicate hash")
    require(
        actual_hashes[-1] == spec["parent_hash"],
        "block-hashes: final entry does not match parent hash",
    )


def validate_payloads(spec: dict[str, Any], payloads: dict[str, Any]) -> dict[str, int]:
    """Validate all payload files and return their checked counts."""
    require(set(payloads) == set(PAYLOAD_FILES), "payload file set mismatch")
    block = payloads["block.json"]
    parent = payloads["parent.json"]
    block_hashes = payloads["block-hashes.json"]
    raw_transactions = payloads["raw-transactions.json"]
    receipts = payloads["receipts.json"]
    proofs = payloads["state-proofs.json"]
    codes = payloads["codes.json"]

    validate_header(block, spec, parent=False)
    validate_header(parent, spec, parent=True)
    validate_block_hashes(spec, block_hashes)

    transactions = block.get("transactions")
    require(isinstance(transactions, list), "block.transactions: expected array")
    expected_hashes = list(spec["transaction_hashes"])
    require(len(transactions) == len(expected_hashes), "block transaction count mismatch")
    for index, (transaction, expected_hash) in enumerate(zip(transactions, expected_hashes)):
        require(isinstance(transaction, dict), f"block transaction {index}: expected object")
        require(transaction.get("hash") == expected_hash, f"transaction {index}: hash mismatch")
        require(
            transaction.get("blockHash") == spec["block_hash"],
            f"transaction {index}: block hash mismatch",
        )
        require(
            quantity(transaction.get("transactionIndex"), f"transaction {index}.index") == index,
            f"transaction {index}: index mismatch",
        )
        require(
            quantity(transaction.get("chainId"), f"transaction {index}.chainId")
            == fixture_chain_id(spec),
            f"transaction {index}: chain id mismatch",
        )

    require(isinstance(raw_transactions, dict), "raw-transactions: expected object")
    require(
        raw_transactions.get("blockHash") == spec["block_hash"],
        "raw-transactions: block hash mismatch",
    )
    raw_entries = raw_transactions.get("transactions")
    require(isinstance(raw_entries, list), "raw-transactions.transactions: expected array")
    require(len(raw_entries) == len(expected_hashes), "raw transaction count mismatch")
    for index, (entry, expected_hash) in enumerate(zip(raw_entries, expected_hashes)):
        require(isinstance(entry, dict), f"raw transaction {index}: expected object")
        require(entry.get("hash") == expected_hash, f"raw transaction {index}: hash mismatch")
        raw = decode_data(entry.get("raw"), f"raw transaction {index}")
        actual_hash = f"0x{keccak256(raw).hex()}"
        require(actual_hash == expected_hash, f"raw transaction {index}: signed hash mismatch")

    require(isinstance(receipts, list), "receipts: expected array")
    require(len(receipts) == len(expected_hashes), "receipt count mismatch")
    log_count = 0
    for index, (receipt, expected_hash) in enumerate(zip(receipts, expected_hashes)):
        require(isinstance(receipt, dict), f"receipt {index}: expected object")
        require(receipt.get("transactionHash") == expected_hash, f"receipt {index}: tx mismatch")
        require(
            quantity(receipt.get("transactionIndex"), f"receipt {index}.index") == index,
            f"receipt {index}: order mismatch",
        )
        require(receipt.get("blockHash") == spec["block_hash"], f"receipt {index}: hash mismatch")
        require(
            quantity(receipt.get("blockNumber"), f"receipt {index}.blockNumber")
            == spec["height"],
            f"receipt {index}: height mismatch",
        )
        require(
            "contractAddress" in receipt and receipt["contractAddress"] is None,
            f"receipt {index}: expected explicit null contractAddress",
        )
        logs = receipt.get("logs")
        require(isinstance(logs, list), f"receipt {index}.logs: expected array")
        log_count += len(logs)
    require(
        receipts[-1].get("cumulativeGasUsed") == spec["gas_used"],
        "final receipt cumulative gas does not equal header gasUsed",
    )

    require(isinstance(proofs, dict), "state-proofs: expected object")
    require(proofs.get("blockHash") == spec["parent_hash"], "state-proofs: block hash mismatch")
    require(
        proofs.get("stateRoot") == spec["parent_state_root"],
        "state-proofs: state root mismatch",
    )
    proof_accounts = proofs.get("accounts")
    require(isinstance(proof_accounts, list), "state-proofs.accounts: expected array")
    require(len(proof_accounts) == len(spec["accounts"]), "state proof account count mismatch")

    require(isinstance(codes, dict), "codes: expected object")
    require(codes.get("blockHash") == spec["parent_hash"], "codes: block hash mismatch")
    code_accounts = codes.get("accounts")
    require(isinstance(code_accounts, list), "codes.accounts: expected array")
    require(len(code_accounts) == len(spec["accounts"]), "code account count mismatch")

    nonempty_code_count = 0
    storage_count = 0
    for index, expected_account in enumerate(spec["accounts"]):
        proof_entry = proof_accounts[index]
        code_entry = code_accounts[index]
        role = expected_account["role"]
        address = expected_account["address"]
        storage_keys = list(expected_account["storage_keys"])
        require(isinstance(proof_entry, dict), f"proof account {index}: expected object")
        require(proof_entry.get("role") == role, f"proof account {index}: role mismatch")
        require(
            proof_entry.get("requestedStorageKeys") == storage_keys,
            f"proof account {index}: requested key set mismatch",
        )
        proof = proof_entry.get("proof")
        require(isinstance(proof, dict), f"proof account {index}: missing proof object")
        require(proof.get("address") == address, f"proof account {index}: address mismatch")
        account_proof = proof.get("accountProof")
        require(
            isinstance(account_proof, list) and bool(account_proof),
            f"proof account {index}: empty account proof",
        )
        storage_proofs = proof.get("storageProof")
        require(isinstance(storage_proofs, list), f"proof account {index}: storage proof missing")
        require(
            [item.get("key") for item in storage_proofs] == storage_keys,
            f"proof account {index}: storage proof key/order mismatch",
        )
        for slot_index, storage_proof in enumerate(storage_proofs):
            require(
                isinstance(storage_proof.get("proof"), list)
                and bool(storage_proof["proof"]),
                f"proof account {index} slot {slot_index}: empty proof",
            )
        storage_count += len(storage_proofs)

        require(isinstance(code_entry, dict), f"code account {index}: expected object")
        require(code_entry.get("role") == role, f"code account {index}: role mismatch")
        require(code_entry.get("address") == address, f"code account {index}: address mismatch")
        code = decode_data(code_entry.get("code"), f"code account {index}")
        code_hash = f"0x{keccak256(code).hex()}"
        proof_is_absent = (
            proof.get("balance") == "0x0"
            and proof.get("nonce") == "0x0"
            and proof.get("codeHash") == ZERO_HASH
            and proof.get("storageHash") == ZERO_HASH
        )
        if proof_is_absent:
            require(not code, f"code account {index}: absent account returned code")
            require(not storage_proofs, f"proof account {index}: absent account returned storage")
        else:
            require(code_hash == proof.get("codeHash"), f"code account {index}: code hash mismatch")
        if code:
            nonempty_code_count += 1
        else:
            require(code_hash == KECCAK_EMPTY, f"code account {index}: bad empty code hash")

    require(
        storage_count == expected_storage_count(spec),
        "state proof storage count mismatch",
    )
    require(
        nonempty_code_count == spec["nonempty_code_count"],
        "nonempty parent code count mismatch",
    )
    return {
        "transactions": len(transactions),
        "receipts": len(receipts),
        "logs": log_count,
        "blockHashes": len(block_hashes),
        "accountProofs": len(proof_accounts),
        "storageProofs": storage_count,
        "codeQueries": len(code_accounts),
        "nonemptyCodes": nonempty_code_count,
    }


def validate_canonical_header(header: Any, number: int, block_hash: str) -> None:
    """Check one exact-number response against its pinned canonical hash."""
    require(isinstance(header, dict), f"canonical height {number}: lookup returned null")
    require(
        quantity(header.get("number"), f"canonical height {number}.number") == number,
        f"canonical height {number}: response height mismatch",
    )
    require(
        header.get("hash") == block_hash,
        f"canonical height {number}: hash mismatch",
    )


def fetch_block_hashes(client: RpcClient, spec: dict[str, Any]) -> list[dict[str, Any]]:
    """Fetch the EVM BLOCKHASH window between canonical child/parent checks."""
    parent_height = spec["height"] - 1
    canonical, canonical_parent = client.batch(
        [
            ("eth_getBlockByNumber", [hex(spec["height"]), False]),
            ("eth_getBlockByNumber", [hex(parent_height), False]),
        ]
    )
    validate_canonical_header(canonical, spec["height"], spec["block_hash"])
    validate_canonical_header(canonical_parent, parent_height, spec["parent_hash"])

    numbers = list(range(spec["height"] - 256, spec["height"]))
    entries = []
    batch_size = spec.get("block_hash_batch_size", 64)
    batch_delay = spec.get("block_hash_batch_delay", 0.0)
    require(type(batch_size) is int and batch_size > 0, "invalid BLOCKHASH batch size")
    require(
        isinstance(batch_delay, (int, float)) and batch_delay >= 0,
        "invalid BLOCKHASH batch delay",
    )
    for offset in range(0, len(numbers), batch_size):
        batch_numbers = numbers[offset : offset + batch_size]
        headers = client.batch(
            [("eth_getBlockByNumber", [hex(number), False]) for number in batch_numbers]
        )
        for number, header in zip(batch_numbers, headers):
            require(isinstance(header, dict), f"BLOCKHASH height {number}: lookup returned null")
            require(
                quantity(header.get("number"), f"BLOCKHASH height {number}.number") == number,
                f"BLOCKHASH height {number}: response height mismatch",
            )
            block_hash = header.get("hash")
            decoded_hash = decode_data(block_hash, f"BLOCKHASH height {number}.hash")
            require(len(decoded_hash) == 32, f"BLOCKHASH height {number}: invalid hash")
            entries.append({"number": number, "hash": block_hash})
        if batch_delay and offset + batch_size < len(numbers):
            time.sleep(batch_delay)
    validate_block_hashes(spec, entries)

    final_canonical, final_parent = client.batch(
        [
            ("eth_getBlockByNumber", [hex(spec["height"]), False]),
            ("eth_getBlockByNumber", [hex(parent_height), False]),
        ]
    )
    validate_canonical_header(final_canonical, spec["height"], spec["block_hash"])
    validate_canonical_header(final_parent, parent_height, spec["parent_hash"])
    return entries


def fetch_payloads(client: RpcClient, spec: dict[str, Any]) -> dict[str, Any]:
    """Fetch one height using exact child, parent, and historical numbers."""
    block, parent, receipts = client.batch(
        [
            ("eth_getBlockByHash", [spec["block_hash"], True]),
            ("eth_getBlockByHash", [spec["parent_hash"], True]),
            ("eth_getBlockReceipts", [spec["block_hash"]]),
        ]
    )
    require(isinstance(block, dict), "block lookup returned null")
    transactions = block.get("transactions")
    require(isinstance(transactions, list), "block transaction list is unavailable")

    raw_values = client.batch(
        [
            ("eth_getRawTransactionByHash", [transaction["hash"]])
            for transaction in transactions
        ]
    )
    raw_transactions = {
        "blockHash": spec["block_hash"],
        "transactions": [
            {"hash": transaction["hash"], "raw": raw}
            for transaction, raw in zip(transactions, raw_values)
        ],
    }

    parent_parameter = {
        "blockHash": spec["parent_hash"],
        "requireCanonical": True,
    }
    proof_code_calls = []
    for account in spec["accounts"]:
        proof_code_calls.append(
            (
                "eth_getProof",
                [account["address"], list(account["storage_keys"]), parent_parameter],
            )
        )
        proof_code_calls.append(("eth_getCode", [account["address"], parent_parameter]))
    proof_batch_size = spec.get("proof_batch_size", len(proof_code_calls))
    proof_batch_delay = spec.get("proof_batch_delay", 0.0)
    require(type(proof_batch_size) is int and proof_batch_size > 0, "invalid proof batch size")
    require(
        isinstance(proof_batch_delay, (int, float)) and proof_batch_delay >= 0,
        "invalid proof batch delay",
    )
    proof_code_results = []
    for offset in range(0, len(proof_code_calls), proof_batch_size):
        proof_code_results.extend(
            client.batch(proof_code_calls[offset : offset + proof_batch_size])
        )
        if proof_batch_delay and offset + proof_batch_size < len(proof_code_calls):
            time.sleep(proof_batch_delay)

    proof_accounts = []
    code_accounts = []
    for index, account in enumerate(spec["accounts"]):
        proof_accounts.append(
            {
                "role": account["role"],
                "requestedStorageKeys": list(account["storage_keys"]),
                "proof": proof_code_results[2 * index],
            }
        )
        code_accounts.append(
            {
                "role": account["role"],
                "address": account["address"],
                "code": proof_code_results[2 * index + 1],
            }
        )

    block_hashes = fetch_block_hashes(client, spec)

    return {
        "block.json": block,
        "parent.json": parent,
        "block-hashes.json": block_hashes,
        "raw-transactions.json": raw_transactions,
        "receipts.json": receipts,
        "state-proofs.json": {
            "blockHash": spec["parent_hash"],
            "stateRoot": spec["parent_state_root"],
            "accounts": proof_accounts,
        },
        "codes.json": {
            "blockHash": spec["parent_hash"],
            "accounts": code_accounts,
        },
    }


def build_manifest(
    fixture_dir: Path,
    spec: dict[str, Any],
    counts: dict[str, int],
    rpc_url: str,
    client_version: str,
    fetched_at: str,
    block_hashes_fetched_at: str | None = None,
    collector_path: Path | None = None,
) -> dict[str, Any]:
    """Build provenance and payload hash metadata."""
    collector_path = collector_path or Path(__file__)
    collector = {
        "path": str(collector_path.relative_to(REPOSITORY_ROOT)),
        "sha256": sha256_file(collector_path),
    }
    shared_path = Path(__file__).resolve()
    if collector_path.resolve() != shared_path:
        collector["sharedPath"] = str(shared_path.relative_to(REPOSITORY_ROOT))
        collector["sharedSha256"] = sha256_file(shared_path)
    files = {}
    for name in PAYLOAD_FILES:
        path = fixture_dir / name
        files[name] = {
            "sha256": sha256_file(path),
            "sizeBytes": path.stat().st_size,
        }
    return {
        "schemaVersion": 1,
        "chainId": fixture_chain_id(spec),
        "height": spec["height"],
        "blockHash": spec["block_hash"],
        "parentHash": spec["parent_hash"],
        "parentStateRoot": spec["parent_state_root"],
        "stateRoot": spec["state_root"],
        "source": {
            "rpcUrl": rpc_url,
            "clientVersion": client_version,
            "fetchedAt": fetched_at,
            "blockHashesFetchedAt": block_hashes_fetched_at or fetched_at,
            "blockSelection": "exact_hash_with_canonical_height_check",
            "parentStateSelection": "eip1898_exact_parent_hash_require_canonical",
            "blockHashSelection": "exact_number_with_parent_and_child_recheck",
        },
        "payloadSemantics": {
            "rpcResultMembers": "verbatim",
            "preserveAbsentNullAndEmpty": True,
            "arrayOrder": "verbatim",
            "blockHashWindow": "ascending_height_minus_256_through_parent",
        },
        "counts": counts,
        "files": files,
        "oracles": {
            "blockFile": {
                "status": "UNAVAILABLE",
                "expectedFixture": None,
                "reason": (
                    "No canonical blockfile exists for this height in the legacy S3 prefix, "
                    "and the official RPC does not expose trace_debankBlock."
                ),
            }
        },
        "validation": {
            "status": "PASS",
            "checks": [
                "chain_id",
                "canonical_block_hash",
                "canonical_block_hash_window",
                "parent_hash_and_state_root",
                "child_state_receipts_and_transactions_roots",
                "ordered_transaction_and_receipt_sets",
                "raw_signed_transaction_hashes",
                "eip1186_requested_key_set",
                "parent_code_hashes",
                "payload_sha256",
            ],
        },
        "collector": collector,
    }


def write_checksums(fixture_dir: Path) -> None:
    """Write conventional SHA256SUMS for manifest and payload files."""
    names = sorted((*PAYLOAD_FILES, "manifest.json"))
    lines = [f"{sha256_file(fixture_dir / name)}  {name}" for name in names]
    (fixture_dir / "SHA256SUMS").write_text("\n".join(lines) + "\n", encoding="utf-8")


def verify_checksums(fixture_dir: Path) -> None:
    """Validate SHA256SUMS and reject missing or extra entries."""
    checksum_path = fixture_dir / "SHA256SUMS"
    try:
        lines = checksum_path.read_text(encoding="utf-8").splitlines()
    except OSError as error:
        raise FixtureError(f"{checksum_path}: cannot read: {error}") from error
    checksums: dict[str, str] = {}
    for line in lines:
        parts = line.split("  ", 1)
        require(len(parts) == 2, f"{checksum_path}: malformed line")
        digest, name = parts
        require(name not in checksums, f"{checksum_path}: duplicate {name}")
        require("/" not in name and name not in ("", ".", ".."), f"{checksum_path}: unsafe path")
        require(len(digest) == 64, f"{checksum_path}: invalid digest for {name}")
        checksums[name] = digest
    expected_names = set((*PAYLOAD_FILES, "manifest.json"))
    require(set(checksums) == expected_names, f"{checksum_path}: file set mismatch")
    for name, expected in checksums.items():
        require(
            sha256_file(fixture_dir / name) == expected,
            f"{fixture_dir / name}: SHA-256 mismatch",
        )


def verify_fixture_dir(fixture_dir: Path, spec: dict[str, Any]) -> dict[str, int]:
    """Perform complete offline verification for one fixture directory."""
    expected_names = set((*PAYLOAD_FILES, "manifest.json", "SHA256SUMS"))
    try:
        actual_names = {path.name for path in fixture_dir.iterdir() if path.is_file()}
    except OSError as error:
        raise FixtureError(f"{fixture_dir}: cannot list fixture: {error}") from error
    require(actual_names == expected_names, f"{fixture_dir}: unexpected fixture file set")

    payloads = {name: read_json(fixture_dir / name) for name in PAYLOAD_FILES}
    counts = validate_payloads(spec, payloads)
    manifest = read_json(fixture_dir / "manifest.json")
    require(isinstance(manifest, dict), f"{fixture_dir}: manifest is not an object")
    expected_manifest_fields = {
        "schemaVersion": 1,
        "chainId": fixture_chain_id(spec),
        "height": spec["height"],
        "blockHash": spec["block_hash"],
        "parentHash": spec["parent_hash"],
        "parentStateRoot": spec["parent_state_root"],
        "stateRoot": spec["state_root"],
    }
    for field, expected in expected_manifest_fields.items():
        require(manifest.get(field) == expected, f"{fixture_dir}: manifest {field} mismatch")
    require(manifest.get("counts") == counts, f"{fixture_dir}: manifest counts mismatch")
    require(
        manifest.get("payloadSemantics", {}).get("preserveAbsentNullAndEmpty") is True,
        f"{fixture_dir}: missing optional/null preservation declaration",
    )
    require(
        manifest.get("payloadSemantics", {}).get("blockHashWindow")
        == "ascending_height_minus_256_through_parent",
        f"{fixture_dir}: missing BLOCKHASH window declaration",
    )
    require(
        manifest.get("source", {}).get("blockHashSelection")
        == "exact_number_with_parent_and_child_recheck",
        f"{fixture_dir}: missing exact-number BLOCKHASH provenance",
    )
    require(
        isinstance(manifest.get("source", {}).get("blockHashesFetchedAt"), str),
        f"{fixture_dir}: missing BLOCKHASH fetch time",
    )
    block_file_oracle = manifest.get("oracles", {}).get("blockFile", {})
    require(
        block_file_oracle.get("status") == "UNAVAILABLE"
        and block_file_oracle.get("expectedFixture") is None,
        f"{fixture_dir}: blockfile oracle must be explicitly unavailable",
    )
    manifest_files = manifest.get("files")
    require(isinstance(manifest_files, dict), f"{fixture_dir}: manifest files missing")
    require(set(manifest_files) == set(PAYLOAD_FILES), f"{fixture_dir}: manifest file set mismatch")
    for name in PAYLOAD_FILES:
        metadata = manifest_files[name]
        path = fixture_dir / name
        require(isinstance(metadata, dict), f"{fixture_dir}: invalid metadata for {name}")
        require(metadata.get("sha256") == sha256_file(path), f"{path}: manifest hash mismatch")
        require(metadata.get("sizeBytes") == path.stat().st_size, f"{path}: size mismatch")
    verify_checksums(fixture_dir)
    return counts


def fixture_spec_by_height(
    fixtures: tuple[dict[str, Any], ...] = FIXTURES,
) -> dict[int, dict[str, Any]]:
    """Index the fixed fixture definitions."""
    return {spec["height"]: spec for spec in fixtures}


def verify_all(
    output_root: Path,
    quiet: bool = False,
    fixtures: tuple[dict[str, Any], ...] = FIXTURES,
) -> None:
    """Verify both frozen heights without network access."""
    require(
        f"0x{keccak256(b'').hex()}" == KECCAK_EMPTY,
        "internal Keccak-256 implementation failed its empty-input vector",
    )
    for spec in fixtures:
        fixture_dir = output_root / str(spec["height"])
        counts = verify_fixture_dir(fixture_dir, spec)
        if not quiet:
            print(
                f"PASS {spec['height']}: "
                f"{counts['transactions']} tx, {counts['receipts']} receipts, "
                f"{counts['blockHashes']} block hashes, "
                f"{counts['accountProofs']} account proofs, "
                f"{counts['storageProofs']} storage proofs, "
                f"{counts['nonemptyCodes']} nonempty codes"
            )


def fetch_all(
    output_root: Path,
    rpc_url: str,
    timeout: float,
    retries: int,
    fixtures: tuple[dict[str, Any], ...] = FIXTURES,
    temporary_prefix: str = ".taiko-mainnet-fixtures-",
    collector_path: Path | None = None,
) -> None:
    """Fetch, validate, and atomically install both fixture directories."""
    existing = [output_root / str(spec["height"]) for spec in fixtures]
    require(
        not any(path.exists() for path in existing),
        "one or more target fixture directories already exist; refusing to overwrite",
    )
    require(
        f"0x{keccak256(b'').hex()}" == KECCAK_EMPTY,
        "internal Keccak-256 implementation failed its empty-input vector",
    )

    client = RpcClient(rpc_url, timeout, retries)
    chain_id, client_version = client.batch(
        [("eth_chainId", []), ("web3_clientVersion", [])]
    )
    expected_chain_ids = {fixture_chain_id(spec) for spec in fixtures}
    require(len(expected_chain_ids) == 1, "fixture group must use exactly one chain id")
    expected_chain_id = next(iter(expected_chain_ids))
    require(quantity(chain_id, "eth_chainId") == expected_chain_id, "RPC chain id mismatch")
    require(isinstance(client_version, str) and client_version, "missing RPC client version")
    fetched_at = utc_now()

    collected: list[tuple[dict[str, Any], dict[str, Any], dict[str, int]]] = []
    for spec in fixtures:
        print(f"FETCH {spec['height']} {spec['block_hash']}")
        payloads = fetch_payloads(client, spec)
        counts = validate_payloads(spec, payloads)
        collected.append((spec, payloads, counts))

    output_root.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=temporary_prefix, dir=output_root.parent) as temporary:
        temporary_root = Path(temporary)
        for spec, payloads, counts in collected:
            fixture_dir = temporary_root / str(spec["height"])
            fixture_dir.mkdir()
            for name in PAYLOAD_FILES:
                write_json(fixture_dir / name, payloads[name])
            manifest = build_manifest(
                fixture_dir,
                spec,
                counts,
                rpc_url,
                client_version,
                fetched_at,
                collector_path=collector_path,
            )
            write_json(fixture_dir / "manifest.json", manifest)
            write_checksums(fixture_dir)
        verify_all(temporary_root, quiet=True, fixtures=fixtures)

        output_root.mkdir(parents=True, exist_ok=True)
        for spec in fixtures:
            (temporary_root / str(spec["height"])).rename(
                output_root / str(spec["height"])
            )
    verify_all(output_root, fixtures=fixtures)


def fetch_block_hashes_for_existing(
    output_root: Path, rpc_url: str, timeout: float, retries: int
) -> None:
    """Add BLOCKHASH windows to existing pinned fixtures without replacing payloads."""
    require(output_root.is_dir(), f"{output_root}: fixture root is unavailable")
    client = RpcClient(rpc_url, timeout, retries)
    chain_id, client_version = client.batch(
        [("eth_chainId", []), ("web3_clientVersion", [])]
    )
    require(quantity(chain_id, "eth_chainId") == CHAIN_ID, "RPC chain id is not Taiko mainnet")
    require(isinstance(client_version, str) and client_version, "missing RPC client version")
    block_hashes_fetched_at = utc_now()
    original_names = tuple(name for name in PAYLOAD_FILES if name != "block-hashes.json")

    collected = []
    for spec in FIXTURES:
        fixture_dir = output_root / str(spec["height"])
        require(fixture_dir.is_dir(), f"{fixture_dir}: fixture directory is unavailable")
        require(
            not (fixture_dir / "block-hashes.json").exists(),
            f"{fixture_dir}: block-hashes.json already exists; refusing to overwrite",
        )
        original_payloads = {name: read_json(fixture_dir / name) for name in original_names}
        original_manifest = read_json(fixture_dir / "manifest.json")
        require(isinstance(original_manifest, dict), f"{fixture_dir}: invalid manifest")
        original_files = original_manifest.get("files")
        require(
            isinstance(original_files, dict) and set(original_files) == set(original_names),
            f"{fixture_dir}: original manifest file set mismatch",
        )
        for name in original_names:
            metadata = original_files[name]
            path = fixture_dir / name
            require(
                isinstance(metadata, dict)
                and metadata.get("sha256") == sha256_file(path)
                and metadata.get("sizeBytes") == path.stat().st_size,
                f"{path}: original manifest metadata mismatch",
            )

        print(f"FETCH BLOCKHASH {spec['height'] - 256}..{spec['height'] - 1}")
        block_hashes = fetch_block_hashes(client, spec)
        payloads = {**original_payloads, "block-hashes.json": block_hashes}
        counts = validate_payloads(spec, payloads)
        fetched_at = original_manifest.get("source", {}).get("fetchedAt")
        require(isinstance(fetched_at, str), f"{fixture_dir}: original fetch time missing")
        collected.append((spec, fixture_dir, payloads, counts, fetched_at))

    with tempfile.TemporaryDirectory(
        prefix=".taiko-mainnet-block-hashes-", dir=output_root.parent
    ) as temporary:
        temporary_root = Path(temporary)
        for spec, fixture_dir, payloads, counts, fetched_at in collected:
            temporary_fixture = temporary_root / str(spec["height"])
            temporary_fixture.mkdir()
            for name in original_names:
                shutil.copy2(fixture_dir / name, temporary_fixture / name)
            write_json(temporary_fixture / "block-hashes.json", payloads["block-hashes.json"])
            manifest = build_manifest(
                temporary_fixture,
                spec,
                counts,
                rpc_url,
                client_version,
                fetched_at,
                block_hashes_fetched_at,
            )
            write_json(temporary_fixture / "manifest.json", manifest)
            write_checksums(temporary_fixture)
        verify_all(temporary_root, quiet=True)

        for spec, fixture_dir, _, _, _ in collected:
            temporary_fixture = temporary_root / str(spec["height"])
            for name in ("block-hashes.json", "manifest.json", "SHA256SUMS"):
                (temporary_fixture / name).replace(fixture_dir / name)
    verify_all(output_root)


def parse_args() -> argparse.Namespace:
    """Parse the explicit fetch or default offline verify command."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "command",
        choices=("fetch", "fetch-block-hashes", "verify"),
        default="verify",
        nargs="?",
        help="fetch commands use the network; verify is offline and is the default",
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
        help=f"RPC URL used only by fetch commands (default: {DEFAULT_RPC_URL})",
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
        default=5,
        help="transport attempts for fetch (default: 5)",
    )
    args = parser.parse_args()
    require(args.timeout > 0, "--timeout must be positive")
    require(args.retries > 0, "--retries must be positive")
    return args


def main() -> int:
    """Run collection or offline verification."""
    try:
        args = parse_args()
        if args.command == "fetch":
            fetch_all(args.output_root.resolve(), args.rpc_url, args.timeout, args.retries)
        elif args.command == "fetch-block-hashes":
            fetch_block_hashes_for_existing(
                args.output_root.resolve(), args.rpc_url, args.timeout, args.retries
            )
        else:
            verify_all(args.output_root.resolve())
        return 0
    except FixtureError as error:
        print(f"FAIL: {error}")
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
