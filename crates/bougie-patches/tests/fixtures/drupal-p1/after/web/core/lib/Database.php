<?php
class Database {
    public function connect() {
        return $this->driver->openWithRetry();
    }
    public function query($sql, array $params = []) {
        return $this->run($sql, $params);
    }
}
