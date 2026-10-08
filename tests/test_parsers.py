import io
import os
import plistlib
import tempfile
import unittest
import zipfile

from idevice_fleet.backups import list_backups
from idevice_fleet.devices import (ecid_key, parse_apple_usb_serial, parse_irecovery_query,
                                   parse_lockdown_plist, scan_sysfs)
from idevice_fleet.firmware import Library, check_download_url, version_key
from idevice_fleet.jobs import parse_percent_line, parse_restore_line


class DeviceParsing(unittest.TestCase):
    def test_usb_serial(self):
        s = "CPID:8101 CPRV:10 CPFM:03 SCEP:01 BDID:14 ECID:001A2B3C4D5E6F78 IBFL:3D SRTG:[iBoot-8419.0.42]"
        f = parse_apple_usb_serial(s)
        self.assertEqual(f["CPID"], "8101")
        self.assertEqual(f["SRTG"], "iBoot-8419.0.42")
        self.assertEqual(ecid_key(int(f["ECID"], 16)), "1a2b3c4d5e6f78")

    def test_irecovery_query(self):
        q = parse_irecovery_query("CPID: 0x8101\nECID: 0x001a2b3c4d5e6f78\nMODE: Recovery\nPRODUCT: iPad13,18\nNAME: iPad (10th generation)\n")
        self.assertEqual(q["PRODUCT"], "iPad13,18")
        self.assertEqual(q["MODE"], "Recovery")
        self.assertEqual(ecid_key(int(q["ECID"], 16)), "1a2b3c4d5e6f78")

    def test_lockdown_plist(self):
        data = plistlib.dumps({"UniqueDeviceID": "00008101-001A2B3C4D5E6F78", "UniqueChipID": 0x1A2B3C4D5E6F78,
                               "DeviceName": "Front desk", "ProductType": "iPad13,18", "ProductVersion": "27.0.1",
                               "SerialNumber": "F9FZ2ABCD34E"})
        d = parse_lockdown_plist(data)
        self.assertEqual(d["ecid"], "1a2b3c4d5e6f78")
        self.assertEqual(d["name"], "Front desk")
        self.assertEqual(d["serial"], "F9FZ2ABCD34E")

    def test_sysfs_scan(self):
        with tempfile.TemporaryDirectory() as root:
            def dev(name, vid, pid, serial):
                os.mkdir(os.path.join(root, name))
                for k, v in (("idVendor", vid), ("idProduct", pid), ("serial", serial)):
                    with open(os.path.join(root, name, k), "w") as f:
                        f.write(v + "\n")
            dev("1-1", "05ac", "1281", "CPID:8101 ECID:00000000000000AB IBFL:3D")
            dev("1-2", "05ac", "1227", "CPID:8030 ECID:00000000000000CD")
            dev("1-3", "05ac", "12a8", "00008101-001A2B3C4D5E6F78")  # normal mode: ignored
            dev("1-4", "046d", "c52b", "")
            found = sorted(scan_sysfs(root), key=lambda d: d["ecid"])
        self.assertEqual([(d["ecid"], d["mode"]) for d in found], [("ab", "Recovery"), ("cd", "DFU")])


class ProgressParsing(unittest.TestCase):
    def test_restore_progress(self):
        self.assertEqual(parse_restore_line("progress: 2 0.453000"), ("Uploading filesystem", 45.3))
        self.assertEqual(parse_restore_line("progress: 4 1.000000"), ("Flashing firmware", 100.0))
        self.assertIsNone(parse_restore_line("Sending iBEC (1234 bytes)..."))

    def test_backup_percent(self):
        self.assertEqual(parse_percent_line("[=============                  ]  42% Finished"), 42.0)
        self.assertIsNone(parse_percent_line("Receiving files"))


class Firmware(unittest.TestCase):
    def test_version_key(self):
        self.assertGreater(version_key("27.0.1"), version_key("27.0"))
        self.assertGreater(version_key("26.10"), version_key("26.9.2"))

    def test_library_reads_manifest(self):
        with tempfile.TemporaryDirectory() as d:
            for name, ver, build in (("a.ipsw", "26.7", "23H31"), ("b.ipsw", "27.0.1", "24A446")):
                buf = io.BytesIO()
                with zipfile.ZipFile(buf, "w") as z:
                    z.writestr("BuildManifest.plist", plistlib.dumps({
                        "ProductVersion": ver, "ProductBuildVersion": build,
                        "SupportedProductTypes": ["iPad13,18", "iPad13,19"]}))
                with open(os.path.join(d, name), "wb") as f:
                    f.write(buf.getvalue())
            with open(os.path.join(d, "broken.ipsw"), "wb") as f:
                f.write(b"not a zip")
            lib = Library(d)
            lib.scan()
            self.assertEqual([e["version"] for e in lib.for_product("iPad13,18")], ["27.0.1", "26.7"])
            self.assertEqual(lib.for_product("iPhone15,2"), [])
            self.assertIn("error", next(e for e in lib.entries if e["file"] == "broken.ipsw"))
            self.assertTrue(lib.resolve("b.ipsw").endswith("b.ipsw"))
            with self.assertRaises(ValueError):
                lib.resolve("../../etc/passwd")

    def test_download_url_allowlist(self):
        ok = "https://updates.cdn-apple.com/2026FallFCS/x/iPad_Fall_2022_27.0.1_24A446_Restore.ipsw"
        self.assertEqual(check_download_url(ok), "iPad_Fall_2022_27.0.1_24A446_Restore.ipsw")
        for bad in ("http://updates.cdn-apple.com/a.ipsw", "https://evil.example.com/a.ipsw",
                    "https://updates.cdn-apple.com/a.zip"):
            with self.assertRaises(ValueError):
                check_download_url(bad)


class Backups(unittest.TestCase):
    def test_list_backups(self):
        with tempfile.TemporaryDirectory() as d:
            folder = os.path.join(d, "00008101-001A2B3C4D5E6F78")
            os.mkdir(folder)
            with open(os.path.join(folder, "Info.plist"), "wb") as f:
                plistlib.dump({"Device Name": "CEO iPhone", "Product Type": "iPhone17,1", "Serial Number": "ABC123"}, f)
            with open(os.path.join(folder, "Manifest.plist"), "wb") as f:
                plistlib.dump({"IsEncrypted": True}, f)
            os.mkdir(os.path.join(d, "not-a-backup"))
            b = list_backups(d)
        self.assertEqual(len(b), 1)
        self.assertEqual(b[0]["device_name"], "CEO iPhone")
        self.assertTrue(b[0]["encrypted"])
        self.assertFalse(b[0]["complete"])


if __name__ == "__main__":
    unittest.main()
