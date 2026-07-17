-- Seed the three Athlet-O lines, each in ready-cup and powder-packet format.

INSERT INTO products (slug, name, description, format, calories, protein_g, price_cents) VALUES
    ('athlet-o-starter-cup', 'Athlet-O Starter',
     'Lime-citrus protein wobble for daily training. 20g gelatin protein, inulin fiber, vitamin C, and electrolytes in a grab-and-go ready cup.',
     'cup', 90, 20, 449),
    ('athlet-o-starter-powder', 'Athlet-O Starter',
     'Lime-citrus protein wobble for daily training. 20g gelatin protein, inulin fiber, vitamin C, and electrolytes -- just add water and chill.',
     'powder', 80, 20, 299),
    ('recover-o-cup', 'Recover-O',
     'Berry-orange recovery wobble for the ride home. Gelatin protein plus magnesium, potassium, vitamin C, fiber, and live cultures in a ready cup.',
     'cup', 90, 22, 499),
    ('recover-o-powder', 'Recover-O',
     'Berry-orange recovery wobble for the ride home. Gelatin protein plus magnesium, potassium, vitamin C, fiber, and live cultures -- just add water and chill.',
     'powder', 80, 22, 329),
    ('pre-game-o-cup', 'Pre-Game-O',
     'Citrus-punch prep cup for pre-game rituals. Sodium, potassium, and vitamin C with gelatin protein and no sugar rush, ready to eat.',
     'cup', 85, 15, 399),
    ('pre-game-o-powder', 'Pre-Game-O',
     'Citrus-punch prep for pre-game rituals. Sodium, potassium, and vitamin C with gelatin protein and no sugar rush -- just add water and chill.',
     'powder', 75, 15, 249);
