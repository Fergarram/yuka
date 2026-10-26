--
-- Desktop
--
-- The settings applications read from the desktop rather than from the compositor: GTK's
-- stylesheets and gsettings. A desktop environment owns these; yuka is the session here,
-- so it owns them too, and its opinion lives in the repo instead of in hand edited files.
--
-- The stylesheets are desktop/gtk-3.0.css and desktop/gtk-4.0.css. Each is copied to
-- ~/.config/gtk-N.0/yuka.css as the scripts load, and that directory's gtk.css is made to
-- start with an @import of it. Nothing else in gtk.css is touched, and what follows the
-- import wins, so it is the place for personal tweaks. Applications read these when they
-- start; one already running keeps what it had.
--
-- desktop/yuka-portals.conf goes to ~/.config/xdg-desktop-portal/, telling the portal which
-- backend serves a yuka session. It is how applications set to follow the system hear the
-- color scheme lua/looks.lua writes.
--
-- The repo is not watched for these files, so after editing one, Super+R reloads.
--

local desktop = {}

--
-- Settings
--

desktop.settings = {
    -- Install the stylesheets from desktop/. false leaves every gtk.css alone.
    gtk_css = true,

    -- Install desktop/yuka-portals.conf. false leaves the portal to its own defaults.
    portals_conf = true,

    -- gsettings applied when yuka starts, as schema, key, and the value in gsettings' own
    -- syntax: strings in single quotes, booleans and numbers bare. Run in the background, so
    -- nothing waits on them. Applications without xdg-desktop-portal read some of these
    -- directly (fonts) and others not at all (color-scheme goes through the portal).
    gsettings = {
        { "org.gnome.desktop.interface", "font-name", "'Cantarell 9'" },
        { "org.gnome.desktop.wm.preferences", "titlebar-font", "'Cantarell 9'" },
        { "org.gnome.desktop.wm.preferences", "titlebar-uses-system-font", "false" },
    },
}

local settings = desktop.settings

--
-- Files
--

local IMPORT = '@import url("yuka.css");'

local function config_dir()
    local xdg = os.getenv("XDG_CONFIG_HOME")
    if xdg and xdg ~= "" then return xdg end
    return (os.getenv("HOME") or "") .. "/.config"
end

local function read(path)
    local file = io.open(path, "r")
    if not file then return nil end
    local text = file:read("*a")
    file:close()
    return text
end

local function write(path, text)
    local file = io.open(path, "w")
    if not file then return false end
    file:write(text)
    file:close()
    return true
end

-- Quoted for the shell, whatever is inside.
local function shell_quote(value)
    return "'" .. string.gsub(value, "'", "'\\''") .. "'"
end

-- Copy a file from the repo into place, only when it differs. false when it could not.
local function install(source_path, dir, name)
    local source = read(source_path)
    if not source then
        print("yuka: desktop: no " .. source_path .. ", skipped")
        return false
    end
    local target = dir .. "/" .. name
    if read(target) == source then return true end
    if write(target, source) then return true end
    os.execute("mkdir -p " .. shell_quote(dir))
    if write(target, source) then return true end
    print("yuka: desktop: cannot write " .. target)
    return false
end

-- Copy one stylesheet into place and make sure gtk.css imports it. Only writes what differs,
-- so a reload with nothing changed touches no file.
local function install_css(version)
    local dir = config_dir() .. "/gtk-" .. version
    if not install("desktop/gtk-" .. version .. ".css", dir, "yuka.css") then return end
    local gtk = dir .. "/gtk.css"
    local current = read(gtk) or ""
    if not string.find(current, IMPORT, 1, true) then
        -- First, because an @import has to come before any rule.
        write(gtk, IMPORT .. "\n\n" .. current)
    end
end

-- Set one gsetting, in the background. For values that change while running, like the color
-- scheme; the ones in the settings table go together at startup.
function desktop.gsettings_set(schema, key, value)
    os.execute("gsettings set " .. shell_quote(schema) .. " " .. shell_quote(key) .. " "
        .. shell_quote(value) .. " >/dev/null 2>&1 &")
end

local function apply_gsettings()
    local list = settings.gsettings
    if #list == 0 then return end
    local cmd = {}
    for i = 1, #list do
        local setting = list[i]
        cmd[i] = "gsettings set " .. shell_quote(setting[1]) .. " " .. shell_quote(setting[2])
            .. " " .. shell_quote(setting[3])
    end
    os.execute("(" .. table.concat(cmd, "; ") .. ") >/dev/null 2>&1 &")
end

--
-- Load and frame
--

if settings.gtk_css then
    install_css("3.0")
    install_css("4.0")
end
if settings.portals_conf then
    install("desktop/yuka-portals.conf", config_dir() .. "/xdg-desktop-portal", "yuka-portals.conf")
end

function desktop.frame(frame)
    if frame.frame == 0 then
        apply_gsettings()
    end
end

return desktop
