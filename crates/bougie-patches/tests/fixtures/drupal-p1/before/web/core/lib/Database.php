<?php
class Database {
    public function connect() {
        return $this->driver->open();
    }
    public function query($sql) {
        return $this->run($sql);
    }
}
