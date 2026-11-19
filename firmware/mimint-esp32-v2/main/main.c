/* Mimint ESP32 v2 — reference firmware skeleton (Phase 7).
 *
 * NOT YET BUILT: ESP-IDF is absent on the build machine, so this
 * file is a skeleton only. It sketches the intended structure:
 * NVS init, identity-key provisioning, Wi-Fi station, and the
 * friends-session handshake stub. It opens NO server of any kind:
 * no HTTP control endpoint, no WS server — the device is a WS
 * client to the hub only. No reboot / upgrade_firmware handling:
 * the hub never calls them.
 */
#include <stdio.h>

void app_main(void) {
    /* TODO (blocked on ESP-IDF): nvs_flash_init, provision the
     * P-256 identity keypair on first boot, connect Wi-Fi as a
     * station, then run the v2 handshake (pinned key + ECDH
     * ephemeral + AES-256-GCM) as a client to the hub and
     * exchange text turns. */
    printf("mimint-esp32-v2 skeleton: not built (ESP-IDF absent)\n");
}
