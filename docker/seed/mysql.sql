-- Demo schema for strut-stack-sql (MySQL)
CREATE DATABASE IF NOT EXISTS demo;
USE demo;

CREATE TABLE users (
    id          INT AUTO_INCREMENT PRIMARY KEY,
    email       VARCHAR(255) NOT NULL UNIQUE,
    name        VARCHAR(255) NOT NULL,
    created_at  TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE products (
    id          INT AUTO_INCREMENT PRIMARY KEY,
    sku         VARCHAR(32) NOT NULL UNIQUE,
    name        VARCHAR(255) NOT NULL,
    price_cents INT NOT NULL,
    CHECK (price_cents >= 0)
);

CREATE TABLE orders (
    id          INT AUTO_INCREMENT PRIMARY KEY,
    user_id     INT NOT NULL,
    product_id  INT NOT NULL,
    quantity    INT NOT NULL,
    status      VARCHAR(32) NOT NULL DEFAULT 'pending',
    created_at  TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (user_id) REFERENCES users(id),
    FOREIGN KEY (product_id) REFERENCES products(id),
    CHECK (quantity > 0)
);

CREATE INDEX idx_orders_user_id ON orders(user_id);
CREATE INDEX idx_orders_status ON orders(status);

DROP PROCEDURE IF EXISTS seed_demo;
DELIMITER //
CREATE PROCEDURE seed_demo()
BEGIN
    DECLARE i INT DEFAULT 1;
    WHILE i <= 200 DO
        INSERT INTO users (email, name)
        VALUES (CONCAT('user', i, '@example.com'), CONCAT('User ', i));
        SET i = i + 1;
    END WHILE;

    SET i = 1;
    WHILE i <= 50 DO
        INSERT INTO products (sku, name, price_cents)
        VALUES (
            CONCAT('SKU-', LPAD(i, 5, '0')),
            CONCAT('Product ', i),
            ((i % 50) + 1) * 100
        );
        SET i = i + 1;
    END WHILE;

    SET i = 1;
    WHILE i <= 1000 DO
        INSERT INTO orders (user_id, product_id, quantity, status)
        VALUES (
            (i % 200) + 1,
            (i % 50) + 1,
            (i % 5) + 1,
            CASE
                WHEN i % 3 = 0 THEN 'shipped'
                WHEN i % 3 = 1 THEN 'pending'
                ELSE 'cancelled'
            END
        );
        SET i = i + 1;
    END WHILE;
END //
DELIMITER ;

CALL seed_demo();
DROP PROCEDURE seed_demo;
