#!/usr/bin/env python3
"""A position's whole life on a Surfpool mainnet fork: open, relever, delever,
close. Fake money only.

Start the fork first (in another terminal):
    surfpool start -u "$RPC_URL" --no-tui --no-deploy

Then, from the repo root:
    cargo build --release
    python3 scripts/fork_lifecycle_test.py kamino --collateral SOL --debt USDC
    python3 scripts/fork_lifecycle_test.py jupiter --vault 1 [--deposit 1 --leverage 2]

A throwaway keypair gets 10 SOL (plus the collateral, if it isn't SOL),
opens the position, rebalances it up to --up and down to --down, then
closes it, all with --send, checking each step lands where it should.
"""
import argparse, base64, os, re, subprocess, sys, tempfile

from fork_delever_test import (
    BIN, FORK, JUP_VAULTS, KLEND, ORACLE_OFFSETS, TOKEN_INFO, account, b58decode, b58encode,
    copy_from_mainnet, refresh_oracles, rpc, set_account, set_token_account, slot,
)

SOL = "So11111111111111111111111111111111111111112"
MAIN_MARKET = "7u3HeHxYDLhnCoErrtycNokbQYbWGzLs6JSDqGAv5PfF"
KNOWN = {"SOL": SOL, "USDC": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
         "JITOSOL": "J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn",
         "CBBTC": "cbbtcf3aa214zXHbiAZQwf4122FBYbraNdFqgw4iMij"}
# Proprietary market makers price from off-chain updates a frozen fork
# doesn't get; stick to classic pools.
DEXES = ["--jupiter-dexes", "Whirlpool,Raydium CLMM,Meteora DLMM"]


def run(*args):
    print("$", " ".join(args))
    out = subprocess.run(args, capture_output=True, text=True)
    print(out.stdout + out.stderr)
    if out.returncode:
        sys.exit("command failed")
    return out.stdout


def decimals(mint):
    return base64.b64decode(account(FORK, mint)["data"][0])[44]


def kamino_reserve(mainnet, market, mint):
    found = rpc(mainnet, "getProgramAccounts", [KLEND, {"encoding": "base64", "filters": [
        {"dataSize": 8624},
        {"memcmp": {"offset": 32, "bytes": market}},
        {"memcmp": {"offset": 128, "bytes": mint}},
        {"memcmp": {"offset": 8 + 4848, "bytes": "1"}}]}])  # config.status 0: active
    if len(found) != 1:
        sys.exit(f"expected one reserve for {mint}, found {len(found)}")
    return found[0]["pubkey"], base64.b64decode(found[0]["account"]["data"][0])


def setup_kamino(mainnet, market, mints):
    """Copy both reserves with their vaults from one slot; return their oracles."""
    snapshot, oracles = set(), set()
    for mint in mints:
        key, r = kamino_reserve(mainnet, market, mint)
        snapshot.add(key)
        for off in (152, 184, 2552, 2592):  # supply / fee vault, collateral mint / vault
            snapshot.add(b58encode(r[8 + off:8 + off + 32]))
        for off in ORACLE_OFFSETS:
            k = r[TOKEN_INFO + off:TOKEN_INFO + off + 32]
            if k != b"\0" * 32:
                oracles.add(b58encode(k))
    copy_from_mainnet(mainnet, snapshot)
    return oracles


def setup_jupiter(mainnet, vault):
    """Return the vault's collateral mint and oracle sources."""
    configs = rpc(mainnet, "getProgramAccounts", [JUP_VAULTS, {"encoding": "base64", "filters": [
        {"dataSize": 8 + 211},
        {"memcmp": {"offset": 8, "bytes": base64.b64encode(vault.to_bytes(2, "little")).decode(),
                    "encoding": "base64"}}]}])
    if len(configs) != 1:
        sys.exit(f"expected one config for vault {vault}, found {len(configs)}")
    c = base64.b64decode(configs[0]["account"]["data"][0])[8:]
    o = base64.b64decode(account(FORK, b58encode(c[18:50]))["data"][0])[8:]
    count = int.from_bytes(o[2:6], "little")
    return b58encode(c[146:178]), {b58encode(o[6 + 66 * i:6 + 66 * i + 32]) for i in range(count)}


def is_empty(position):
    acc = account(FORK, position)
    if acc is None:
        return True  # klend closes an emptied obligation and refunds its rent
    data = base64.b64decode(acc["data"][0])
    if acc["owner"] == KLEND:
        # First deposit and borrow slots are cleared when fully withdrawn / repaid.
        return data[8 + 88:8 + 120] == b"\0" * 32 and data[8 + 1200:8 + 1232] == b"\0" * 32
    p = data[8:]
    return p[38] == 1 and int.from_bytes(p[47:55], "little") == 0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("venue", choices=["kamino", "jupiter"])
    ap.add_argument("--collateral", default="SOL")
    ap.add_argument("--debt", default="USDC")
    ap.add_argument("--market", default=MAIN_MARKET)
    ap.add_argument("--vault", type=int, default=1)
    ap.add_argument("--deposit", default="1")
    ap.add_argument("--leverage", default="2")
    ap.add_argument("--up", default="2.5", help="relever to this after opening")
    ap.add_argument("--down", default="1.5", help="then delever to this")
    ap.add_argument("--ripcord", default="3.5", help="must sit above --up")
    args = ap.parse_args()
    mainnet = os.environ.get("RPC_URL") or sys.exit("set RPC_URL to a mainnet RPC")
    drift = slot(mainnet) - slot(FORK)
    if drift > 300:
        sys.exit(f"the fork is {drift} slots behind mainnet; restart surfpool and rerun")

    tmp = tempfile.mkdtemp()
    key = os.path.join(tmp, "owner.json")
    subprocess.run(["solana-keygen", "new", "--no-bip39-passphrase", "--silent", "--force",
                    "-o", key], check=True, capture_output=True)
    owner = subprocess.check_output(["solana-keygen", "pubkey", key], text=True).strip()
    print("throwaway owner:", owner)
    set_account(owner, b"", 10 * 10**9, "11111111111111111111111111111111")

    if args.venue == "kamino":
        mints = [KNOWN.get(t.upper(), t) for t in (args.collateral, args.debt)]
        # Catch the fork up first: a reserve copied from a slot the fork hasn't
        # reached makes klend's slot math overflow.
        refresh_oracles(mainnet, set())
        oracles = setup_kamino(mainnet, args.market, mints)
        coll = mints[0]
        # klend checks price age against the fork's clock, which slot time
        # travel pushes ahead of real time; copied prices are enough.
        refresh = lambda: copy_from_mainnet(mainnet, oracles)
        pick = ["--venue", "kamino", "--market", args.market,
                "--collateral", args.collateral, "--debt", args.debt]
    else:
        coll, oracles = setup_jupiter(mainnet, args.vault)
        # Jupiter's oracle rejects prices posted at a slot the fork hasn't reached.
        refresh = lambda: refresh_oracles(mainnet, oracles)
        pick = ["--venue", "jupiter", "--vault", str(args.vault)]
    if coll != SOL:
        set_token_account(owner, coll, int(float(args.deposit) * 10 ** decimals(coll)) + 1)

    common = ["--rpc-url", FORK, "--keypair", key]
    refresh()
    out = run(BIN, "open", *pick, "--deposit", args.deposit, "--leverage", args.leverage,
              *common, "--send", *DEXES)
    position = re.search(r"^position: (\w+)", out, re.M).group(1)
    table = re.search(r"lookup table: (\w+)", out).group(1)
    leverage = float(re.search(r": ([\d.]+)× \(liquidation", out).group(1))
    ok = abs(leverage / float(args.leverage) - 1) < 0.03
    print(f"{'PASS' if ok else 'FAIL'} open: {leverage:.3f}× (asked {args.leverage}×)")

    position_args = ["--position", position, "--lookup-table", table, *common]
    for target in (args.up, args.down):
        refresh()
        out = run(BIN, "rebalance", *position_args, "--target", target, "--band", "0.1",
                  "--ripcord", args.ripcord, "--send", *DEXES)
        after = float(re.search(r"leverage now ([\d.]+)×", out).group(1))
        # Delevers repay only the swap's guaranteed output, so they land a
        # little above target.
        good = abs(after / float(target) - 1) < 0.02
        ok &= good
        print(f"{'PASS' if good else 'FAIL'} rebalance: {after:.3f}× (target {target}×)")

    refresh()
    run(BIN, "close", *position_args, "--send", *DEXES)
    closed = is_empty(position)
    ok &= closed
    print(f"{'PASS' if closed else 'FAIL'} close: position {'empty' if closed else 'still holds funds'}")
    lamports = rpc(FORK, "getBalance", [owner])["value"]
    print(f"wallet: {lamports / 1e9:.6f} SOL (started with 10)")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
