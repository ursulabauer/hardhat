import { network } from "@ignored/hardhat-vnext";
import { expect } from "chai";

import "./setup.js";

import { HardhatEthers } from "@ignored/hardhat-vnext-ethers/types";

describe("Lock", function () {
  let networkHelpers: any; // TODO: We need to export this type in @ignored/hardhat-vnext-network-helpers
  let ethers: HardhatEthers;

  before(async function () {
    const connection = await network.connect();
    ethers = connection.ethers;
    networkHelpers = connection.networkHelpers;
  });

  // We define a fixture to reuse the same setup in every test.
  // We use loadFixture to run this setup once, snapshot that state,
  // and reset Hardhat Network to that snapshot in every test.
  async function deployOneYearLockFixture() {
    const ONE_YEAR_IN_SECS = 365 * 24 * 60 * 60;
    const ONE_GWEI = 1_000_000_000;

    const lockedAmount = ONE_GWEI;

    const latestTime = await networkHelpers.time.latest();
    const unlockTime = latestTime + ONE_YEAR_IN_SECS;

    // Contracts are deployed using the first signer/account by default
    const [owner, otherAccount] = await ethers.getSigners();

    const Lock = await ethers.getContractFactory("Lock");
    const lock = await Lock.deploy(unlockTime, { value: lockedAmount });

    return { lock, unlockTime, lockedAmount, owner, otherAccount };
  }

  describe("Deployment", function () {
    it("Should set the right unlockTime", async function () {
      const { lock, unlockTime } = await networkHelpers.loadFixture(
        deployOneYearLockFixture,
      );

      expect(await lock.unlockTime()).to.equal(BigInt(unlockTime));
    });

    it("Should set the right owner", async function () {
      const { lock, owner } = await networkHelpers.loadFixture(
        deployOneYearLockFixture,
      );

      expect(await lock.owner()).to.equal(owner.address);
    });

    it("Should receive and store the funds to lock", async function () {
      const { lock, lockedAmount } = await networkHelpers.loadFixture(
        deployOneYearLockFixture,
      );

      expect(await ethers.provider.getBalance(lock.target)).to.equal(
        BigInt(lockedAmount),
      );
    });

    it("Should fail if the unlockTime is not in the future", async function () {
      // We don't use the fixture here because we want a different deployment
      const latestTime = await networkHelpers.time.latest();
      const Lock = await ethers.getContractFactory("Lock");

      // TODO: bring back the original test assertion `hardhat-chai-matchers`
      // is available with `revertedWith`.
      await expect(
        Lock.deploy(latestTime, { value: 1 }),
      ).to.eventually.be.rejectedWith("Unlock time should be in the future");
    });
  });

  describe("Withdrawals", function () {
    describe("Validations", function () {
      it("Should revert with the right error if called too soon", async function () {
        const { lock } = await networkHelpers.loadFixture(
          deployOneYearLockFixture,
        );

        await expect(lock.withdraw()).to.eventually.be.rejectedWith(
          "You can't withdraw yet",
        );
      });

      it("Should revert with the right error if called from another account", async function () {
        const { lock, unlockTime, otherAccount } =
          await networkHelpers.loadFixture(deployOneYearLockFixture);

        // We can increase the time in Hardhat Network
        await networkHelpers.time.increaseTo(unlockTime);

        // We use lock.connect() to send a transaction from another account
        await expect(
          (lock.connect(otherAccount) as any).withdraw(),
        ).to.eventually.be.rejectedWith("You aren't the owner");
      });

      it("Shouldn't fail if the unlockTime has arrived and the owner calls it", async function () {
        const { lock, unlockTime } = await networkHelpers.loadFixture(
          deployOneYearLockFixture,
        );

        // Transactions are sent using the first signer by default
        await networkHelpers.time.increaseTo(unlockTime);

        await expect(lock.withdraw()).to.eventually.be.fulfilled;
      });
    });

    describe("Events", function () {
      // TODO: bring back the original test once `hardhat-chai-matchers`
      // is available for asserting on events.
      it("Should emit an event on withdrawals", async function () {
        const { lock, unlockTime, lockedAmount } =
          await networkHelpers.loadFixture(deployOneYearLockFixture);

        await networkHelpers.time.increaseTo(unlockTime);

        const withdrawResult = await lock.withdraw();

        const receipt = await withdrawResult.wait();

        expect(receipt.logs.length).to.equal(1);
        expect(
          receipt.logs.filter((l: any) => l.fragment.name === "Withdrawal")
            .length,
        ).to.equal(1);
      });
    });

    describe("Transfers", function () {
      // TODO: bring back the original Transfers test once
      // `hardhat-chai-matchers` has been ported.
      it("Should transfer the funds out of the timelock", async function () {
        const { lock, unlockTime } = await networkHelpers.loadFixture(
          deployOneYearLockFixture,
        );

        await networkHelpers.time.increaseTo(unlockTime);

        await lock.withdraw();

        const afterLockedBalance = await ethers.provider.getBalance(lock);

        expect(afterLockedBalance).to.equal(0n);
      });
    });
  });
});
