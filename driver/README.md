# Virtual display driver

XDisplay uses the open-source [Virtual Display Driver](https://github.com/VirtualDrivers/Virtual-Display-Driver)
(MttVDD, MIT licensed, Microsoft-signed) to give Windows a real extra monitor. The signed x64 driver
package is redistributed in `installer/redist/vdd/` and installed by the Windows installer.

| File | Purpose |
|---|---|
| `vdd_settings.xml` | The display modes the driver offers: common phone, tablet and foldable resolutions in landscape (16:9, 16:10, 5:3, 3:2, 7:5, 4:3, 18:9 to 21:9), each at 60, 75, 90, 120 and 144 Hz. The installer copies it to `C:\VirtualDisplayDriver` and points the driver at it (`HKLM\SOFTWARE\MikeTheTech\VirtualDisplayDriver\VDDPATH`). |
| `enable_virtual_display.ps1` | For source checkouts: installs/restarts the driver and applies the mode list (run as administrator). |

The host attaches the monitor only while a device is connected, at the device's own resolution
when it is in the list (otherwise the closest shape), and detaches it afterwards. To add a
resolution, add a `<resolution>` entry (landscape, width >= height) and reinstall or restart the
driver.

The host can also manage the driver directly (administrator rights needed):

```powershell
xdisplay-host.exe --install-display-driver path\to\MttVDD.inf   # exit 0, or 3010 = restart needed
xdisplay-host.exe --query-display-driver                        # exit 0 when the device exists
xdisplay-host.exe --uninstall-display-driver
```
