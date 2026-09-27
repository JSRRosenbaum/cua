-- Agent keyboards own independent keymaps; never change the human layout.
-- This package-owned template is copied into Omarchy's existing user toggles
-- only by `cua-omarchy input on` after ABI and mapped-module checks.
hl.config({ plugin = { cua = { enabled = true } } })

-- A module cannot survive logout. Recheck its consumer profile at startup;
-- do not retain an enabled flag after a failed fresh-session admission.
hl.on("hyprland.start", function()
  hl.exec_cmd("cua-omarchy input load")
end)
