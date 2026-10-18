#!/usr/bin/env python3
"""End-to-end rebalance test on a Surfpool mainnet fork. Fake money only.

Start the fork first (in another terminal):
    surfpool start -u "$RPC_URL" --no-tui --no-deploy

Then, from the repo root:
    cargo build --release
    python3 scripts/fork_delever_test.py <KAMINO_OBLIGATION> [--up 2.2] [--target 1.4]

It copies the mainnet obligation into the fork, hands it to a throwaway
keypair, funds that keypair, creates its token accounts and lookup table,
refreshes oracle prices (the fork's copies go stale), then relevers up to
--up and delevers down to --target with --send, checking each lands.
"""
import argparse, base64, json, os, re, subprocess, sys, tempfile, urllib.request

FORK = "http://127.0.0.1:8899"
ALPH = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
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


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("obligation")
    ap.add_argument("--up", default="2.2", help="relever to this leverage first")
    ap.add_argument("--target", default="1.4", help="then delever to this leverage")
    args = ap.parse_args()
    mainnet = os.environ.get("RPC_URL") or sys.exit("set RPC_URL to a mainnet RPC")

    tmp = tempfile.mkdtemp()
    key = os.path.join(tmp, "owner.json")
    subprocess.run(["solana-keygen", "new", "--no-bip39-passphrase", "--silent", "--force",
                    "-o", key], check=True, capture_output=True)
    owner = subprocess.check_output(["solana-keygen", "pubkey", key], text=True).strip()
    print("throwaway owner:", owner)

    obl = account(FORK, args.obligation) or sys.exit("obligation not found")
    data = bytearray(base64.b64decode(obl["data"][0]))
    data[OWNER_OFFSET:OWNER_OFFSET + 32] = b58decode(owner)
    set_account(args.obligation, data, obl["lamports"], obl["owner"])
    set_account(owner, b"", 10 * 10**9, "11111111111111111111111111111111")

    # The obligation's reserves: first active deposit and borrow.
    deposit = b58encode(bytes(data[8 + 88:8 + 120]))
    borrow = b58encode(bytes(data[8 + 1200:8 + 1232]))
    for reserve in (deposit, borrow):
        r = base64.b64decode(account(FORK, reserve)["data"][0])
        mint = b58encode(r[8 + 120:8 + 152])
        rpc(FORK, "surfnet_setTokenAccount", [owner, mint, {"amount": 0}])

    common = ["--rpc-url", FORK, "--position", args.obligation, "--keypair", key]
    alt = re.search(r"lookup table: (\w+)", run(BIN, "lookup-table", *common)).group(1)

    oracles = set()
    for reserve in (deposit, borrow):
        r = base64.b64decode(account(FORK, reserve)["data"][0])
        for off in ORACLE_OFFSETS:
            k = r[TOKEN_INFO + off:TOKEN_INFO + off + 32]
            if k != b"\0" * 32:
                oracles.add(b58encode(k))

    def refresh_oracles():
        for k in oracles:
            v = account(mainnet, k)
            if v:
                set_account(k, base64.b64decode(v["data"][0]), v["lamports"], v["owner"],
                            v["executable"])

    ok = True
    for target, direction in ((args.up, "up"), (args.target, "down")):
        refresh_oracles()
        # Proprietary market makers (e.g. Kipseli, HumidiFi) price from off-chain
        # updates that a frozen fork doesn't get; stick to classic pools here.
        out = run(BIN, "rebalance", *common, "--target", target, "--band", "0.1",
                  "--ripcord", "4.0", "--lookup-table", alt, "--send",
                  "--jupiter-dexes", "Whirlpool,Raydium CLMM,Meteora DLMM")
        m = re.search(r"leverage now ([\d.]+)× \(was ([\d.]+)×\)", out)
        if not m:
            sys.exit("no leverage line in output")
        after, before = float(m.group(1)), float(m.group(2))
        moved = after > before if direction == "up" else after < before
        good = moved and abs(after - float(target)) < 0.05
        ok &= good
        print(f"{'PASS' if good else 'FAIL'} {direction}: leverage {before:.3f}× → {after:.3f}× (target {target}×)")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
