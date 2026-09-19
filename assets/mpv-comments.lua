-- mpv-comments.lua: YouTubeコメントオーバーレイ + ライブ弾幕表示
-- cキーでコメントon/off、j/kでスクロール
-- dキーで弾幕on/off

local utils = require("mp.utils")

-- === 共通 ===
local overlay = mp.create_osd_overlay("ass-events")
local comment_file = "/tmp/fbrowse-comments.json"
local danmaku_file = "/tmp/fbrowse-danmaku.json"

-- 絵文字除去
local function strip_emoji(s)
    s = s:gsub("[\xf0-\xf4][\x80-\xbf][\x80-\xbf][\x80-\xbf]", "")
    s = s:gsub("\xe2[\x98-\x9e][\x80-\xbf]", "")
    s = s:gsub("\xef\xb8[\x80-\x8f]", "")
    return s
end

-- ASSエスケープ
local function ass_escape(s)
    s = strip_emoji(s)
    s = s:gsub("\\", "\\\\")
    s = s:gsub("{", "\\{")
    s = s:gsub("}", "\\}")
    s = s:gsub("\n", "\\N")
    return s
end

-- === コメントパネル ===
local comments = {}
local comment_visible = false
local scroll_offset = 0
local lines_per_page = 12

local function load_comments()
    local f = io.open(comment_file, "r")
    if not f then return false end
    local content = f:read("*all")
    f:close()
    comments = {}
    for author, text, likes in content:gmatch('"author"%s*:%s*"(.-)".-"text"%s*:%s*"(.-)".-"likes"%s*:%s*"(.-)"') do
        author = author:gsub('\\"', '"'):gsub('\\n', '\n'):gsub('\\\\', '\\')
        text = text:gsub('\\"', '"'):gsub('\\n', '\n'):gsub('\\\\', '\\')
        table.insert(comments, {author = author, text = text, likes = likes})
    end
    return #comments > 0
end

-- === 弾幕 ===
local danmaku_active = {}   -- 画面上を流れているコメント
local danmaku_visible = false
local danmaku_last_id = 0   -- 最後に読み込んだコメントID
local danmaku_speed = 6      -- 秒で画面を横断
local danmaku_rows = 8       -- 行数
local danmaku_row_used = {}  -- 各行の使用状況（最後のコメントの右端がまだ画面内か）

local function load_danmaku()
    local f = io.open(danmaku_file, "r")
    if not f then return end
    local content = f:read("*all")
    f:close()

    local now = mp.get_time()
    local w = mp.get_osd_size()
    if not w or w == 0 then w = 1280 end

    local id = 0
    for text in content:gmatch('"text"%s*:%s*"(.-)"') do
        id = id + 1
        if id > danmaku_last_id then
            text = text:gsub('\\"', '"'):gsub('\\n', ' '):gsub('\\\\', '\\')
            -- 空き行を探す
            local row = nil
            for r = 1, danmaku_rows do
                if not danmaku_row_used[r] or danmaku_row_used[r] < now then
                    row = r
                    break
                end
            end
            if not row then
                row = (id % danmaku_rows) + 1
            end
            -- テキスト幅を推定（1文字あたり約font_sizeの0.6倍）
            local font_size = 56
            local text_width = #ass_escape(text) * font_size * 0.6
            -- この行が次に空く時刻を記録
            danmaku_row_used[row] = now + (text_width / (w + text_width)) * danmaku_speed + 0.5

            table.insert(danmaku_active, {
                text = ass_escape(text),
                row = row,
                start_time = now,
            })
        end
    end
    danmaku_last_id = id
end

-- === 統合描画 ===
local function render_all()
    local w, h = mp.get_osd_size()
    if not w or w == 0 then w = 1280 end
    if not h or h == 0 then h = 720 end
    overlay.res_x = w
    overlay.res_y = h

    local ass = ""

    -- コメントパネル描画
    if comment_visible and #comments > 0 then
        local panel_x = math.floor(w * 0.68)
        local panel_w = w - panel_x
        local panel_y = 40
        local panel_h = h - 80
        local font_size = math.floor(h / 30)
        local line_h = font_size + 4

        -- 背景パネル
        ass = ass .. string.format(
            "{\\pos(%d,%d)\\bord0\\shad0\\1c&H000000&\\1a&H40&\\p1}m 0 0 l %d 0 l %d %d l 0 %d{\\p0}\n",
            panel_x, panel_y, panel_w, panel_w, panel_h, panel_h
        )
        -- ヘッダー
        local total = #comments
        local page_start = scroll_offset + 1
        local page_end = math.min(scroll_offset + lines_per_page, total)
        ass = ass .. string.format(
            "{\\pos(%d,%d)\\fs%d\\bord0\\shad0\\1c&HFFFFFF&\\1a&H00&\\b1}コメント %d-%d / %d{\\b0}\n",
            panel_x + 10, panel_y + 8, font_size, page_start, page_end, total
        )
        -- コメント
        local y = panel_y + 8 + line_h + 10
        for i = page_start, page_end do
            local c = comments[i]
            if not c then break end
            if y + line_h * 2 > panel_y + panel_h then break end
            local author_text = ass_escape(c.author)
            if c.likes ~= "0" and c.likes ~= "" then
                author_text = author_text .. string.format("  ♥%s", c.likes)
            end
            ass = ass .. string.format(
                "{\\pos(%d,%d)\\fs%d\\bord0\\shad0\\1c&HFFFFFF&\\1a&H00&}%s\n",
                panel_x + 10, y, font_size - 2, author_text
            )
            y = y + line_h - 2
            local text = ass_escape(c.text)
            local max_chars = math.floor(panel_w / (font_size * 0.4)) * 3
            if #text > max_chars then
                text = text:sub(1, max_chars) .. "..."
            end
            ass = ass .. string.format(
                "{\\pos(%d,%d)\\fs%d\\bord0\\shad0\\1c&HFFFFFF&\\1a&H00&\\fscx90}%s\n",
                panel_x + 10, y, font_size - 4, text
            )
            y = y + line_h + 6
        end
        -- ヒント
        ass = ass .. string.format(
            "{\\pos(%d,%d)\\fs%d\\bord0\\shad0\\1c&HFFFFFF&\\1a&H00&}c:閉じる  j/k:スクロール\n",
            panel_x + 10, panel_y + panel_h - line_h, font_size - 6
        )
    end

    -- 弾幕描画
    if danmaku_visible then
        local now = mp.get_time()
        local font_size = 56
        local row_h = math.floor(h / danmaku_rows)
        local alive = {}
        for _, d in ipairs(danmaku_active) do
            local elapsed = now - d.start_time
            if elapsed < danmaku_speed then
                -- 右端(w)から左端(-テキスト幅)まで移動
                local text_width = #d.text * font_size * 0.6
                local x = w - (w + text_width) * (elapsed / danmaku_speed)
                local y = (d.row - 1) * row_h + 10
                ass = ass .. string.format(
                    "{\\pos(%d,%d)\\fs%d\\bord1\\shad0\\1c&HFFFFFF&\\1a&H00&\\3c&H000000&}%s\n",
                    math.floor(x), math.floor(y), font_size, d.text
                )
                table.insert(alive, d)
            end
        end
        danmaku_active = alive
    end

    overlay.data = ass
    overlay:update()
end

-- === コメント操作 ===
local function toggle_comments()
    if not comment_visible then
        if #comments == 0 then
            if not load_comments() then
                mp.osd_message("コメントなし", 2)
                return
            end
        end
        comment_visible = true
        scroll_offset = 0
        mp.osd_message("コメント ON", 1)
    else
        comment_visible = false
        mp.osd_message("コメント OFF", 1)
    end
    render_all()
end

local function scroll_down()
    if not comment_visible then return end
    if scroll_offset + lines_per_page < #comments then
        scroll_offset = scroll_offset + lines_per_page
        render_all()
    end
end

local function scroll_up()
    if not comment_visible then return end
    scroll_offset = math.max(0, scroll_offset - lines_per_page)
    render_all()
end

-- === 弾幕操作 ===
local danmaku_timer = nil

local function toggle_danmaku()
    if not danmaku_visible then
        danmaku_visible = true
        -- last_idはリセットしない（既読分を再表示しない）
        danmaku_row_used = {}
        -- アニメーションタイマー開始（20fps）
        if not danmaku_timer then
            danmaku_timer = mp.add_periodic_timer(0.05, function()
                if danmaku_visible then
                    render_all()
                end
            end)
        else
            danmaku_timer:resume()
        end
        mp.osd_message("弾幕 ON", 1)
    else
        danmaku_visible = false
        danmaku_active = {}
        if danmaku_timer then
            danmaku_timer:kill()
            danmaku_timer = nil
        end
        render_all()
        mp.osd_message("弾幕 OFF", 1)
    end
end

-- コメントファイル監視
mp.add_periodic_timer(3, function()
    if comment_visible then
        local old_count = #comments
        load_comments()
        if #comments ~= old_count then
            render_all()
        end
    end
end)

-- 弾幕ファイル監視
mp.add_periodic_timer(2, function()
    if danmaku_visible then
        load_danmaku()
    end
end)

-- キーバインド
mp.add_key_binding("c", "toggle-comments", toggle_comments)
mp.add_key_binding("j", "comments-down", scroll_down)
mp.add_key_binding("k", "comments-up", scroll_up)
mp.add_key_binding("d", "toggle-danmaku", toggle_danmaku)
