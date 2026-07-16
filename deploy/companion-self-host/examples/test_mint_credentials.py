import argparse
import unittest

import mint_credentials


def admission_args(role: str) -> argparse.Namespace:
    return argparse.Namespace(
        admission_ttl=120,
        app_id="app_a",
        subject_id="device_a" if role == "mobile" else "plugin_a",
        role=role,
        holder_key_thumbprint="mobile_holder_a" if role == "mobile" else "plugin_holder_a",
        expected_peer_key_thumbprint=("plugin_holder_a" if role == "mobile" else None),
        approved_peer_key_thumbprint=(
            [] if role == "mobile" else ["mobile_holder_b", "mobile_holder_a"]
        ),
        peer_roster_revision=None if role == "mobile" else 7,
        scope=["state_read", "rtc_signal"],
    )


class MintCredentialsTests(unittest.TestCase):
    def test_mobile_admission_binds_distinct_holder_and_expected_plugin(self) -> None:
        token, claims = mint_credentials.mint_admission(
            b"a" * 32, admission_args("mobile"), 1_000
        )

        self.assertTrue(token.startswith("aokie-adm-v2."))
        self.assertEqual(claims["holderKeyThumbprint"], "mobile_holder_a")
        self.assertEqual(claims["expectedPeerKeyThumbprint"], "plugin_holder_a")
        self.assertNotIn("approvedPeerKeyThumbprints", claims)
        self.assertNotIn("peerRosterRevision", claims)
        self.assertNotIn("peerRosterHash", claims)

    def test_plugin_admission_sorts_and_hashes_approved_mobile_roster(self) -> None:
        _, claims = mint_credentials.mint_admission(
            b"a" * 32, admission_args("plugin"), 1_000
        )

        peers = ["mobile_holder_a", "mobile_holder_b"]
        self.assertEqual(claims["holderKeyThumbprint"], "plugin_holder_a")
        self.assertEqual(claims["approvedPeerKeyThumbprints"], peers)
        self.assertEqual(claims["peerRosterRevision"], 7)
        self.assertEqual(
            claims["peerRosterHash"],
            "0rYAdwWGO3lROTyUoqzxnkWhUYmSOJZIo_dBTP-NZ2o",
        )
        self.assertNotIn("expectedPeerKeyThumbprint", claims)

    def test_mobile_admission_rejects_missing_or_self_peer(self) -> None:
        args = admission_args("mobile")
        args.expected_peer_key_thumbprint = None
        with self.assertRaisesRegex(ValueError, "require"):
            mint_credentials.mint_admission(b"a" * 32, args, 1_000)

        args.expected_peer_key_thumbprint = args.holder_key_thumbprint
        with self.assertRaisesRegex(ValueError, "different"):
            mint_credentials.mint_admission(b"a" * 32, args, 1_000)

    def test_plugin_admission_rejects_missing_roster_or_holder_in_roster(self) -> None:
        args = admission_args("plugin")
        args.approved_peer_key_thumbprint = []
        with self.assertRaisesRegex(ValueError, "at least one"):
            mint_credentials.mint_admission(b"a" * 32, args, 1_000)

        args.approved_peer_key_thumbprint = [args.holder_key_thumbprint]
        with self.assertRaisesRegex(ValueError, "must not appear"):
            mint_credentials.mint_admission(b"a" * 32, args, 1_000)


if __name__ == "__main__":
    unittest.main()
