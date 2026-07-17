-- AthletO rebrand: every product renders the AthletO wordmark with a short
-- sub-name (starter / recover / pre-game) instead of hyphenated line names.

ALTER TABLE products ADD COLUMN IF NOT EXISTS subname text;

UPDATE products SET subname = 'starter'  WHERE slug LIKE 'athlet-o-starter%' OR slug LIKE '%starter%';
UPDATE products SET subname = 'recover'  WHERE slug LIKE 'recover%';
UPDATE products SET subname = 'pre-game' WHERE slug LIKE 'pre-game%' OR slug LIKE 'pregame%';

UPDATE products SET name = 'AthletO';
