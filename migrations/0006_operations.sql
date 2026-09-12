ALTER TABLE orbit_workers ADD COLUMN IF NOT EXISTS draining boolean NOT NULL DEFAULT false;
