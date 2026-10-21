AirBridge - Windows Firewall setup
==================================

AirBridge needs two inbound rules to receive files and to answer local
discovery probes:

  AirBridge File Transfer (TCP)
      Opens the receiver port (default 42517) on Private and Domain
      networks. When AirBridge.exe is found next to the scripts folder,
      the rule is restricted to that program.

  AirBridge mDNS Discovery (UDP 5353)
      Allows multicast DNS replies so nearby devices can resolve this
      machine's address. This rule is not restricted to a program because
      Windows shares the mDNS port with other services.

Public networks are deliberately not opened.

Run it from the app
-------------------
Open AirBridge, find the Sharing card, and choose "Set up". Windows shows a
UAC prompt and the script runs elevated. The app checks the rules again a few
seconds later.

Run it manually
---------------
From the folder that contains this file:

  powershell -ExecutionPolicy Bypass -File .\Setup-Firewall.ps1 -Port 42517

If the current shell is not elevated, the script launches an elevated copy
and exits. Use "-Port" if you changed the transfer port in Settings.

Remove the rules
----------------
  powershell -ExecutionPolicy Bypass -File .\Setup-Firewall.ps1 -Remove

Manual alternative
------------------
These are the equivalent netsh commands:

  netsh advfirewall firewall delete rule name="AirBridge File Transfer (TCP)"
  netsh advfirewall firewall delete rule name="AirBridge mDNS Discovery (UDP 5353)"

  netsh advfirewall firewall add rule name="AirBridge File Transfer (TCP)" ^
      dir=in action=allow protocol=TCP localport=42517 ^
      profile=private,domain program="C:\path\to\AirBridge.exe" enable=yes

  netsh advfirewall firewall add rule name="AirBridge mDNS Discovery (UDP 5353)" ^
      dir=in action=allow protocol=UDP localport=5353 ^
      profile=private,domain enable=yes

Notes
-----
* Changing the transfer port in Settings requires running the script again.
* If a rule already exists, the script deletes and recreates it, so repeated
  runs are safe.
* If transfers still fail, confirm both devices are on the same network and
  that the network profile is Private rather than Public.
