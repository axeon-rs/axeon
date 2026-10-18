CREATE INDEX accounts_balance_idx ON accounts (balance);
INSERT INTO accounts (id, owner, balance) VALUES (1, 'system', 100);
