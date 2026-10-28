#!/usr/bin/env python3
"""Create the ignored synthetic Meteora fixtures if they are absent."""
from pathlib import Path
import subprocess
import sys

root = Path(__file__).resolve().parents[2]
fixtures = root / ".context" / "meteora-validator"
required = ("public-clone-manifest.json", "synthetic-identities.json", "wallet-account.json", "wsol-ata.json", "usdc-ata.json")
if not all((fixtures / name).is_file() for name in required):
    subprocess.run([sys.executable, str(root / "scripts" / "verify-meteora-validator.py"), "--setup"], cwd=root, check=True)
