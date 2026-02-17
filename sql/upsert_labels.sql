INSERT INTO labels (id, name, type, color_foreground, color_background)
VALUES (?, ?, ?, ?, ?)
ON CONFLICT(id) DO UPDATE SET name=excluded.name, type=excluded.type,
color_foreground=excluded.color_foreground, color_background=excluded.color_background
