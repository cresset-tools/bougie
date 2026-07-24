# A Mage-OS store in one command

Bougie is set up really well to work with Mage-OS and Magento.
Setting up a new store is as simple as one command:

```sh
bougie new bougie-store --starter mageos --start
```

This will start all the services you need to run Mage-OS and make sure the store is fully installed.
After this single command you will be able to visit your store at http://bougie-store.bougie.run:7080

## Related projects

That one command pulls together a few separate projects. If you want to understand what happens under the hood, or contribute to any part of it, here is where each piece lives:

- The starter pack is produced by [mageos-maker](https://github.com/cresset-tools/mageos-maker).
- Modules come from the [Modulargento composer repo](https://modulargento.cresset.tools).
