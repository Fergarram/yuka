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
-- The repo is not watched for these files, so after editing one, Super+R reloads.
--

local desktop = {}

--
-- Tuning
--

desktop.tune = {
    -- Install the stylesheets from desktop/. false leaves every gtk.css alone.
    gtk_css = true,

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

local tune = desktop.tune

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
    local f = io.open(path, "r")
    if not f then return nil end
    local text = f:read("*a")
    f:close()
    return text
end

local function write(path, text)
    local f = io.open(path, "w")
    if not f then return false end
    f:write(text)
    f:close()
    return true
end

-- Quoted for the shell, whatever is inside.
local function sh(s)
    return "'" .. string.gsub(s, "'", "'\\''") .. "'"
end

-- Copy one stylesheet into place and make sure gtk.css imports it. Only writes what differs,
-- so a reload with nothing changed touches no file.
local function install_css(version)
    local source = read("desktop/gtk-" .. version .. ".css")
    if not source then
        print("yuka: desktop: no desktop/gtk-" .. version .. ".css, skipped")
        return
    end
    local dir = config_dir() .. "/gtk-" .. version
    local ours = dir .. "/yuka.css"
    if read(ours) ~= source and not write(ours, source) then
        os.execute("mkdir -p " .. sh(dir))
        if not write(ours, source) then
            print("yuka: desktop: cannot write " .. ours)
            return
        end
    end
    local gtk = dir .. "/gtk.css"
    local current = read(gtk) or ""
    if not string.find(current, IMPORT, 1, true) then
        -- First, because an @import has to come before any rule.
        write(gtk, IMPORT .. "\n\n" .. current)
    end
end

local function apply_gsettings()
    local list = tune.gsettings
    if #list == 0 then return end
    local cmd = {}
    for i = 1, #list do
        local s = list[i]
        cmd[i] = "gsettings set " .. sh(s[1]) .. " " .. sh(s[2]) .. " " .. sh(s[3])
    end
    os.execute("(" .. table.concat(cmd, "; ") .. ") >/dev/null 2>&1 &")
end

--
-- Load and frame
--

if tune.gtk_css then
    install_css("3.0")
    install_css("4.0")
end

function desktop.frame(f)
    if f.frame == 0 then
        apply_gsettings()
    end
end

return desktop
