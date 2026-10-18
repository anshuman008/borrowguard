#!/usr/bin/env python3
"""End-to-end rebalance test on a Surfpool mainnet fork. Fake money only.

Start the fork first (in another terminal):
    surfpool start -u "$RPC_URL" --no-tui --no-deploy

Then, from the repo root:
    cargo build --release
    python3 scripts/fork_delever_test.py <POSITION> [--up 2.2] [--target 1.4] [--ripcord 4]

POSITION is a Kamino obligation or a Jupiter Lend position. The script hands
it to a throwaway keypair inside the fork (Kamino: rewrites the obligation
owner; Jupiter Lend: moves the position NFT), funds that keypair, creates its
token accounts and lookup table, refreshes oracle prices (the fork's copies go
stale), then relevers up to --up and delevers down to --target with --send,
checking each lands.
"""
import argparse, base64, json, os, re, subprocess, sys, tempfile, urllib.request

FORK = "http://127.0.0.1:8899"
ALPH = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
KLEND = "KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD"
JUP_VAULTS = "jupr81YtYssSyPt8jbnGuiWon5f6x9TcDEFxYe3Bdzi"
TOKEN_2022 = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb"
OWNER_OFFSET = 64               # Obligation.owner in the account bytes
TOKEN_INFO = 8 + 4848 + 176     # Reserve.config.token_info
ORACLE_OFFSETS = (80, 128, 160, 192)  # scope, switchboard price/twap, pyth
BIN = "./target/release/borrowguard"


def b58encode(b: bytes) -> str:
    n, s = int.from_bytes(b, "big"), ""
    while n:
        n, r = divmod(n, 58)
        s = ALPH[r] + s
    return "1" * (len(b) - len(b.lstrip(b"\0"))) + s


def b58decode(s: str) -> bytes:
    n = 0
    for c in s:
        n = n * 58 + ALPH.index(c)
    return n.to_bytes(32, "big")


def rpc(url, method, params):
    req = urllib.request.Request(url, data=json.dumps(
        {"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode(),
        headers={"content-type": "application/json"})
    out = json.load(urllib.request.urlopen(req, timeout=120))
    if "error" in out:
        sys.exit(f"{method}: {out['error']}")
    return out["result"]


def account(url, key):
    return rpc(url, "getAccountInfo", [key, {"encoding": "base64"}])["value"]


def set_account(key, data, lamports, owner, executable=False):
    rpc(FORK, "surfnet_setAccount", [key, {"data": data.hex(), "lamports": lamports,
                                           "owner": owner, "executable": executable}])


def run(*args):
    print("$", " ".join(args))
    out = subprocess.run(args, capture_output=True, text=True)
    print(out.stdout + out.stderr)
    if out.returncode:
        sys.exit("command failed")
    return out.stdout


def copy_from_mainnet(mainnet, keys):
    """Copy accounts in one call, so they come from the same mainnet slot."""
    keys = sorted(keys)
    for k, v in zip(keys, rpc(mainnet, "getMultipleAccounts", [keys, {"encoding": "base64"}])["value"]):
        if v:
            set_account(k, base64.b64decode(v["data"][0]), v["lamports"], v["owner"],
                        v["executable"])


def slot(url):
    return rpc(url, "getSlot", [])


def refresh_oracles(mainnet, oracles):
    """Copy fresh prices. The fork makes slots slower than mainnet, so first
    move it to mainnet's slot: oracle programs reject prices posted at a
    slot the fork hasn't reached yet."""
    now = slot(mainnet)
    if slot(FORK) < now:
        rpc(FORK, "surfnet_timeTravel", [{"absoluteSlot": now}])
    copy_from_mainnet(mainnet, oracles)


def set_token_account(owner, mint, amount):
    params = [owner, mint, {"amount": amount}]
    if account(FORK, mint)["owner"] == TOKEN_2022:
        params.append(TOKEN_2022)
    rpc(FORK, "surfnet_setTokenAccount", params)


def setup_kamino(obligation, owner, mainnet):
    """Hand the obligation to `owner`; return the oracle accounts to keep fresh."""
    obl = account(FORK, obligation) or sys.exit("obligation not found")
    data = bytearray(base64.b64decode(obl["data"][0]))
    data[OWNER_OFFSET:OWNER_OFFSET + 32] = b58decode(owner)
    set_account(obligation, data, obl["lamports"], obl["owner"])

    # The obligation's reserves: first active deposit and borrow.
    deposit = b58encode(bytes(data[8 + 88:8 + 120]))
    borrow = b58encode(bytes(data[8 + 1200:8 + 1232]))
    # The fork copies accounts lazily, one by one; a reserve and its vaults
    # copied at different slots disagree, and klend's balance checks underflow.
    snapshot = set()
    for reserve in (deposit, borrow):
        r = base64.b64decode(account(FORK, reserve)["data"][0])
        snapshot.add(reserve)
        for off in (152, 184, 2552, 2592):  # supply / fee vault, collateral mint / vault
            snapshot.add(b58encode(r[8 + off:8 + off + 32]))
    copy_from_mainnet(mainnet, snapshot)

    oracles = set()
    for reserve in (deposit, borrow):
        r = base64.b64decode(account(FORK, reserve)["data"][0])
        set_token_account(owner, b58encode(r[8 + 120:8 + 152]), 0)
        for off in ORACLE_OFFSETS:
            k = r[TOKEN_INFO + off:TOKEN_INFO + off + 32]
            if k != b"\0" * 32:
                oracles.add(b58encode(k))
    return oracles


def setup_jupiter(position, owner, mainnet):
    """Move the position NFT to `owner`; return the oracle sources to keep fresh."""
    pos = account(FORK, position) or sys.exit("position not found")
    p = base64.b64decode(pos["data"][0])[8:]
    nft = b58encode(p[6:38])
    # Take the NFT from whoever holds it: the mainnet owner, or an earlier run.
    for url in (mainnet, FORK):
        for holder in rpc(url, "getTokenLargestAccounts", [nft])["value"]:
            if holder["amount"] == "1":
                t = account(FORK, holder["address"])
                set_token_account(b58encode(base64.b64decode(t["data"][0])[32:64]), nft, 0)
    set_token_account(owner, nft, 1)

    # The vault's config: the one VaultConfig-sized account with this vault id.
    configs = rpc(mainnet, "getProgramAccounts", [JUP_VAULTS, {"encoding": "base64", "filters": [
        {"dataSize": 8 + 211},
        {"memcmp": {"offset": 8, "bytes": base64.b64encode(p[0:2]).decode(),
                    "encoding": "base64"}}]}])
    if len(configs) != 1:
        sys.exit(f"expected one vault config, found {len(configs)}")
    c = base64.b64decode(account(FORK, configs[0]["pubkey"])["data"][0])[8:]
    for off in (146, 178):  # supply and borrow mints
        set_token_account(owner, b58encode(c[off:off + 32]), 0)
    o = base64.b64decode(account(FORK, b58encode(c[18:50]))["data"][0])[8:]
    count = int.from_bytes(o[2:6], "little")
    return {b58encode(o[6 + 66 * i:6 + 66 * i + 32]) for i in range(count)}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("position")
    ap.add_argument("--up", default="2.2", help="relever to this leverage first")
    ap.add_argument("--target", default="1.4", help="then delever to this leverage")
    ap.add_argument("--ripcord", default="4.0", help="must sit above both targets")
    args = ap.parse_args()
    mainnet = os.environ.get("RPC_URL") or sys.exit("set RPC_URL to a mainnet RPC")

    # Catching up a drifted fork also moves its clock ahead of real time,
    # which makes prices look stale; a recently started fork has little drift.
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
    program = (account(FORK, args.position) or sys.exit("position not found"))["owner"]
    if program == KLEND:
        oracles = setup_kamino(args.position, owner, mainnet)
    elif program == JUP_VAULTS:
        oracles = setup_jupiter(args.position, owner, mainnet)
    else:
        sys.exit(f"{args.position} is owned by {program}, not Kamino or Jupiter Lend")

    common = ["--rpc-url", FORK, "--position", args.position, "--keypair", key]
    alt = re.search(r"lookup table: (\w+)", run(BIN, "lookup-table", *common)).group(1)

    ok = True
    for target, direction in ((args.up, "up"), (args.target, "down")):
        refresh_oracles(mainnet, oracles)
        # Proprietary market makers (e.g. Kipseli, HumidiFi) price from off-chain
        # updates that a frozen fork doesn't get; stick to classic pools here.
        out = run(BIN, "rebalance", *common, "--target", target, "--band", "0.1",
                  "--ripcord", args.ripcord, "--lookup-table", alt, "--send",
                  "--jupiter-dexes", "Whirlpool,Raydium CLMM,Meteora DLMM")
        m = re.search(r"leverage now ([\d.]+)× \(was ([\d.]+)×\)", out)
        if not m:
            sys.exit("no leverage line in output")
        after, before = float(m.group(1)), float(m.group(2))
        moved = after > before if direction == "up" else after < before
        # Delevers repay only the swap's guaranteed output (slippage floor), so
        # they land a little above target; more so at high leverage.
        good = moved and abs(after / float(target) - 1) < 0.02
        ok &= good
        print(f"{'PASS' if good else 'FAIL'} {direction}: leverage {before:.3f}× → {after:.3f}× (target {target}×)")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
