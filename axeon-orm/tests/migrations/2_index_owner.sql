CREATE INDEX accounts_owner_idx ON accounts (owner);
INSERT INTO accounts (id, owner, balance) VALUES (1, 'system', 100);
