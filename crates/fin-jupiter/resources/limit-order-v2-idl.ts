export type LimitOrder2 = {
  address: "j1o2qRpjcyUwEvwtcfhEQefh773ZgjxcVRry7LDqg5X";
  metadata: {
    name: "limitOrder2";
    version: "0.1.0";
    spec: "0.1.0";
    description: "Created with Anchor";
  };
  instructions: [
    {
      name: "cancelDustOrder";
      discriminator: [197, 112, 189, 164, 79, 48, 23, 246];
      accounts: [
        {
          name: "signer";
          signer: true;
        },
        {
          name: "maker";
          writable: true;
          relations: ["order"];
        },
        {
          name: "order";
          writable: true;
        },
        {
          name: "inputMintReserve";
          writable: true;
        },
        {
          name: "makerInputMintAccount";
          writable: true;
        },
        {
          name: "inputMint";
        },
        {
          name: "inputTokenProgram";
        },
        {
          name: "systemProgram";
          address: "11111111111111111111111111111111";
        },
        {
          name: "associatedTokenProgram";
          address: "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
        },
        {
          name: "eventAuthority";
          pda: {
            seeds: [
              {
                kind: "const";
                value: [
                  95,
                  95,
                  101,
                  118,
                  101,
                  110,
                  116,
                  95,
                  97,
                  117,
                  116,
                  104,
                  111,
                  114,
                  105,
                  116,
                  121
                ];
              }
            ];
          };
        },
        {
          name: "program";
        }
      ];
      args: [
        {
          name: "params";
          type: {
            defined: {
              name: "cancelDustOrderParams";
            };
          };
        }
      ];
    },
    {
      name: "cancelOrder";
      discriminator: [95, 129, 237, 240, 8, 49, 223, 132];
      accounts: [
        {
          name: "signer";
          signer: true;
        },
        {
          name: "maker";
          writable: true;
          relations: ["order"];
        },
        {
          name: "order";
          writable: true;
        },
        {
          name: "inputMintReserve";
          writable: true;
        },
        {
          name: "makerInputMintAccount";
          writable: true;
          optional: true;
        },
        {
          name: "inputMint";
        },
        {
          name: "inputTokenProgram";
        },
        {
          name: "eventAuthority";
          pda: {
            seeds: [
              {
                kind: "const";
                value: [
                  95,
                  95,
                  101,
                  118,
                  101,
                  110,
                  116,
                  95,
                  97,
                  117,
                  116,
                  104,
                  111,
                  114,
                  105,
                  116,
                  121
                ];
              }
            ];
          };
        },
        {
          name: "program";
        }
      ];
      args: [];
    },
    {
      name: "fillOrder";
      discriminator: [232, 122, 115, 25, 199, 143, 136, 162];
      accounts: [
        {
          name: "taker";
          writable: true;
          signer: true;
        },
        {
          name: "maker";
          writable: true;
          relations: ["order"];
        },
        {
          name: "order";
          writable: true;
        },
        {
          name: "takerInputMintAccount";
          writable: true;
        },
        {
          name: "takerOutputMintAccount";
          writable: true;
        },
        {
          name: "makerOutputMintAccount";
          writable: true;
          optional: true;
        },
        {
          name: "feeAccount";
          writable: true;
        },
        {
          name: "orderInputMintAccount";
          writable: true;
        },
        {
          name: "inputMint";
        },
        {
          name: "inputTokenProgram";
        },
        {
          name: "outputMint";
        },
        {
          name: "outputTokenProgram";
        },
        {
          name: "jupiterProgram";
          address: "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4";
        },
        {
          name: "systemProgram";
          address: "11111111111111111111111111111111";
        },
        {
          name: "eventAuthority";
          pda: {
            seeds: [
              {
                kind: "const";
                value: [
                  95,
                  95,
                  101,
                  118,
                  101,
                  110,
                  116,
                  95,
                  97,
                  117,
                  116,
                  104,
                  111,
                  114,
                  105,
                  116,
                  121
                ];
              }
            ];
          };
        },
        {
          name: "program";
        }
      ];
      args: [
        {
          name: "params";
          type: {
            defined: {
              name: "fillOrderParams";
            };
          };
        }
      ];
    },
    {
      name: "flashFillOrder";
      discriminator: [252, 104, 18, 134, 164, 78, 18, 140];
      accounts: [
        {
          name: "taker";
          writable: true;
          signer: true;
        },
        {
          name: "maker";
          writable: true;
          relations: ["order"];
        },
        {
          name: "order";
          writable: true;
        },
        {
          name: "inputMintReserve";
          writable: true;
        },
        {
          name: "makerOutputMintAccount";
          writable: true;
          optional: true;
        },
        {
          name: "takerOutputMintAccount";
          writable: true;
          optional: true;
        },
        {
          name: "feeAccount";
          writable: true;
        },
        {
          name: "inputTokenProgram";
        },
        {
          name: "outputMint";
        },
        {
          name: "outputTokenProgram";
        },
        {
          name: "systemProgram";
          address: "11111111111111111111111111111111";
        },
        {
          name: "eventAuthority";
          pda: {
            seeds: [
              {
                kind: "const";
                value: [
                  95,
                  95,
                  101,
                  118,
                  101,
                  110,
                  116,
                  95,
                  97,
                  117,
                  116,
                  104,
                  111,
                  114,
                  105,
                  116,
                  121
                ];
              }
            ];
          };
        },
        {
          name: "program";
        }
      ];
      args: [
        {
          name: "params";
          type: {
            defined: {
              name: "flashFillOrderParams";
            };
          };
        }
      ];
    },
    {
      name: "initializeOrder";
      discriminator: [133, 110, 74, 175, 112, 159, 245, 159];
      accounts: [
        {
          name: "payer";
          writable: true;
          signer: true;
        },
        {
          name: "maker";
          signer: true;
        },
        {
          name: "order";
          writable: true;
          pda: {
            seeds: [
              {
                kind: "const";
                value: [111, 114, 100, 101, 114];
              },
              {
                kind: "account";
                path: "maker";
              },
              {
                kind: "arg";
                path: "uniqueId";
              }
            ];
          };
        },
        {
          name: "inputMintReserve";
          writable: true;
          pda: {
            seeds: [
              {
                kind: "account";
                path: "order";
              },
              {
                kind: "account";
                path: "inputTokenProgram";
              },
              {
                kind: "account";
                path: "inputMint";
              }
            ];
            program: {
              kind: "const";
              value: [
                140,
                151,
                37,
                143,
                78,
                36,
                137,
                241,
                187,
                61,
                16,
                41,
                20,
                142,
                13,
                131,
                11,
                90,
                19,
                153,
                218,
                255,
                16,
                132,
                4,
                142,
                123,
                216,
                219,
                233,
                248,
                89
              ];
            };
          };
        },
        {
          name: "makerInputMintAccount";
          writable: true;
        },
        {
          name: "fee";
          pda: {
            seeds: [
              {
                kind: "const";
                value: [102, 101, 101];
              }
            ];
          };
        },
        {
          name: "referral";
          optional: true;
        },
        {
          name: "inputMint";
        },
        {
          name: "outputMint";
        },
        {
          name: "inputTokenProgram";
        },
        {
          name: "outputTokenProgram";
        },
        {
          name: "systemProgram";
          address: "11111111111111111111111111111111";
        },
        {
          name: "associatedTokenProgram";
          address: "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
        },
        {
          name: "eventAuthority";
          pda: {
            seeds: [
              {
                kind: "const";
                value: [
                  95,
                  95,
                  101,
                  118,
                  101,
                  110,
                  116,
                  95,
                  97,
                  117,
                  116,
                  104,
                  111,
                  114,
                  105,
                  116,
                  121
                ];
              }
            ];
          };
        },
        {
          name: "program";
        }
      ];
      args: [
        {
          name: "params";
          type: {
            defined: {
              name: "initializeOrderParams";
            };
          };
        }
      ];
    },
    {
      name: "preFlashFillOrder";
      discriminator: [240, 47, 153, 68, 13, 190, 225, 42];
      accounts: [
        {
          name: "taker";
          signer: true;
        },
        {
          name: "order";
          writable: true;
        },
        {
          name: "inputMintReserve";
          writable: true;
        },
        {
          name: "takerInputMintAccount";
          writable: true;
        },
        {
          name: "inputMint";
        },
        {
          name: "inputTokenProgram";
        },
        {
          name: "instruction";
          address: "Sysvar1nstructions1111111111111111111111111";
        }
      ];
      args: [
        {
          name: "params";
          type: {
            defined: {
              name: "preFlashFillOrderParams";
            };
          };
        }
      ];
    },
    {
      name: "updateFee";
      discriminator: [232, 253, 195, 247, 148, 212, 73, 222];
      accounts: [
        {
          name: "admin";
          writable: true;
          signer: true;
        },
        {
          name: "feeAuthority";
          writable: true;
          pda: {
            seeds: [
              {
                kind: "const";
                value: [102, 101, 101];
              }
            ];
          };
        },
        {
          name: "systemProgram";
          address: "11111111111111111111111111111111";
        }
      ];
      args: [
        {
          name: "params";
          type: {
            defined: {
              name: "updateFeeParams";
            };
          };
        }
      ];
    },
    {
      name: "withdrawFee";
      discriminator: [14, 122, 231, 218, 31, 238, 223, 150];
      accounts: [
        {
          name: "admin";
          writable: true;
          signer: true;
        },
        {
          name: "feeAuthority";
          docs: ["CHECK"];
          pda: {
            seeds: [
              {
                kind: "const";
                value: [102, 101, 101];
              }
            ];
          };
        },
        {
          name: "programFeeAccount";
          writable: true;
        },
        {
          name: "adminTokenAcocunt";
          writable: true;
        },
        {
          name: "tokenProgram";
        },
        {
          name: "mint";
        }
      ];
      args: [];
    }
  ];
  accounts: [
    {
      name: "fee";
      discriminator: [24, 55, 150, 250, 168, 27, 101, 178];
    },
    {
      name: "order";
      discriminator: [134, 173, 223, 185, 77, 86, 28, 51];
    }
  ];
  events: [
    {
      name: "cancelDustOrderEvent";
      discriminator: [124, 190, 74, 28, 177, 40, 200, 220];
    },
    {
      name: "cancelOrderEvent";
      discriminator: [174, 66, 141, 17, 4, 224, 162, 77];
    },
    {
      name: "createOrderEvent";
      discriminator: [49, 142, 72, 166, 230, 29, 84, 84];
    },
    {
      name: "tradeEvent";
      discriminator: [189, 219, 127, 211, 78, 230, 97, 238];
    }
  ];
  errors: [
    {
      code: 6000;
      name: "invalidMakingAmount";
    },
    {
      code: 6001;
      name: "invalidTakingAmount";
    },
    {
      code: 6002;
      name: "invalidMaxTakingAmount";
    },
    {
      code: 6003;
      name: "invalidInputAccount";
    },
    {
      code: 6004;
      name: "invalidMakerOutputAccount";
    },
    {
      code: 6005;
      name: "invalidTakerOutputAccount";
    },
    {
      code: 6006;
      name: "invalidPair";
    },
    {
      code: 6007;
      name: "orderExpired";
    },
    {
      code: 6008;
      name: "orderNotExpired";
    },
    {
      code: 6009;
      name: "invalidAdmin";
    },
    {
      code: 6010;
      name: "invalidKeeper";
    },
    {
      code: 6011;
      name: "mathOverflow";
    },
    {
      code: 6012;
      name: "programMismatch";
    },
    {
      code: 6013;
      name: "unknownInstruction";
    },
    {
      code: 6014;
      name: "missingRepayInstructions";
    },
    {
      code: 6015;
      name: "invalidOrder";
    },
    {
      code: 6016;
      name: "invalidBorrowMakingAmount";
    },
    {
      code: 6017;
      name: "invalidExpiredDate";
    },
    {
      code: 6018;
      name: "feeBpsTooLow";
    },
    {
      code: 6019;
      name: "feeBpsTooHigh";
    },
    {
      code: 6020;
      name: "transferTaxMint";
    },
    {
      code: 6021;
      name: "invalidOutputMint";
    },
    {
      code: 6022;
      name: "invalidMinTakingAmount";
    },
    {
      code: 6023;
      name: "invalidSwapData";
    },
    {
      code: 6024;
      name: "invalidSwapInputAmount";
    },
    {
      code: 6025;
      name: "invalidSwappedAmount";
    },
    {
      code: 6026;
      name: "invalidDustOrder";
    },
    {
      code: 6027;
      name: "unsupportedMint";
    }
  ];
  types: [
    {
      name: "cancelDustOrderEvent";
      type: {
        kind: "struct";
        fields: [
          {
            name: "orderKey";
            type: "pubkey";
          },
          {
            name: "oneUsdInputMintAmount";
            type: "u64";
          }
        ];
      };
    },
    {
      name: "cancelDustOrderParams";
      type: {
        kind: "struct";
        fields: [
          {
            name: "oneUsdInputMintAmount";
            type: "u64";
          }
        ];
      };
    },
    {
      name: "cancelOrderEvent";
      type: {
        kind: "struct";
        fields: [
          {
            name: "orderKey";
            type: "pubkey";
          }
        ];
      };
    },
    {
      name: "createOrderEvent";
      type: {
        kind: "struct";
        fields: [
          {
            name: "orderKey";
            type: "pubkey";
          },
          {
            name: "maker";
            type: "pubkey";
          },
          {
            name: "inputMint";
            type: "pubkey";
          },
          {
            name: "outputMint";
            type: "pubkey";
          },
          {
            name: "inputTokenProgram";
            type: "pubkey";
          },
          {
            name: "outputTokenProgram";
            type: "pubkey";
          },
          {
            name: "makingAmount";
            type: "u64";
          },
          {
            name: "takingAmount";
            type: "u64";
          },
          {
            name: "expiredAt";
            type: {
              option: "i64";
            };
          },
          {
            name: "feeBps";
            type: "u16";
          },
          {
            name: "feeAccount";
            type: "pubkey";
          },
          {
            name: "slippageBps";
            type: "u16";
          }
        ];
      };
    },
    {
      name: "fee";
      type: {
        kind: "struct";
        fields: [
          {
            name: "normalFeeBps";
            type: "u16";
          },
          {
            name: "stableFeeBps";
            type: "u16";
          }
        ];
      };
    },
    {
      name: "fillOrderParams";
      type: {
        kind: "struct";
        fields: [
          {
            name: "inputAmount";
            type: "u64";
          },
          {
            name: "swapData";
            type: "bytes";
          }
        ];
      };
    },
    {
      name: "flashFillOrderParams";
      type: {
        kind: "struct";
        fields: [
          {
            name: "maxTakingAmount";
            type: "u64";
          }
        ];
      };
    },
    {
      name: "initializeOrderParams";
      type: {
        kind: "struct";
        fields: [
          {
            name: "uniqueId";
            type: "u64";
          },
          {
            name: "makingAmount";
            type: "u64";
          },
          {
            name: "takingAmount";
            type: "u64";
          },
          {
            name: "expiredAt";
            type: {
              option: "i64";
            };
          },
          {
            name: "feeBps";
            type: {
              option: "u16";
            };
          },
          {
            name: "slippageBps";
            type: {
              option: "u16";
            };
          }
        ];
      };
    },
    {
      name: "order";
      type: {
        kind: "struct";
        fields: [
          {
            name: "maker";
            type: "pubkey";
          },
          {
            name: "inputMint";
            type: "pubkey";
          },
          {
            name: "outputMint";
            type: "pubkey";
          },
          {
            name: "inputTokenProgram";
            type: "pubkey";
          },
          {
            name: "outputTokenProgram";
            type: "pubkey";
          },
          {
            name: "inputMintReserve";
            type: "pubkey";
          },
          {
            name: "uniqueId";
            type: "u64";
          },
          {
            name: "oriMakingAmount";
            type: "u64";
          },
          {
            name: "oriTakingAmount";
            type: "u64";
          },
          {
            name: "makingAmount";
            type: "u64";
          },
          {
            name: "takingAmount";
            type: "u64";
          },
          {
            name: "borrowMakingAmount";
            type: "u64";
          },
          {
            name: "expiredAt";
            type: {
              option: "i64";
            };
          },
          {
            name: "feeBps";
            type: "u16";
          },
          {
            name: "feeAccount";
            type: "pubkey";
          },
          {
            name: "createdAt";
            type: "i64";
          },
          {
            name: "updatedAt";
            type: "i64";
          },
          {
            name: "bump";
            type: "u8";
          },
          {
            name: "slippageBps";
            type: "u16";
          }
        ];
      };
    },
    {
      name: "preFlashFillOrderParams";
      type: {
        kind: "struct";
        fields: [
          {
            name: "makingAmount";
            type: "u64";
          }
        ];
      };
    },
    {
      name: "tradeEvent";
      type: {
        kind: "struct";
        fields: [
          {
            name: "orderKey";
            type: "pubkey";
          },
          {
            name: "taker";
            type: "pubkey";
          },
          {
            name: "remainingMakingAmount";
            type: "u64";
          },
          {
            name: "remainingTakingAmount";
            type: "u64";
          },
          {
            name: "makingAmount";
            type: "u64";
          },
          {
            name: "takingAmount";
            type: "u64";
          }
        ];
      };
    },
    {
      name: "updateFeeParams";
      type: {
        kind: "struct";
        fields: [
          {
            name: "normalFeeBps";
            type: "u16";
          },
          {
            name: "stableFeeBps";
            type: "u16";
          }
        ];
      };
    }
  ];
};

export const IDL: LimitOrder2 = {
  address: "j1o2qRpjcyUwEvwtcfhEQefh773ZgjxcVRry7LDqg5X",
  metadata: {
    name: "limitOrder2",
    version: "0.1.0",
    spec: "0.1.0",
    description: "Created with Anchor",
  },
  instructions: [
    {
      name: "cancelDustOrder",
      discriminator: [197, 112, 189, 164, 79, 48, 23, 246],
      accounts: [
        {
          name: "signer",
          signer: true,
        },
        {
          name: "maker",
          writable: true,
          relations: ["order"],
        },
        {
          name: "order",
          writable: true,
        },
        {
          name: "inputMintReserve",
          writable: true,
        },
        {
          name: "makerInputMintAccount",
          writable: true,
        },
        {
          name: "inputMint",
        },
        {
          name: "inputTokenProgram",
        },
        {
          name: "systemProgram",
          address: "11111111111111111111111111111111",
        },
        {
          name: "associatedTokenProgram",
          address: "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL",
        },
        {
          name: "eventAuthority",
          pda: {
            seeds: [
              {
                kind: "const",
                value: [
                  95, 95, 101, 118, 101, 110, 116, 95, 97, 117, 116, 104, 111,
                  114, 105, 116, 121,
                ],
              },
            ],
          },
        },
        {
          name: "program",
        },
      ],
      args: [
        {
          name: "params",
          type: {
            defined: {
              name: "cancelDustOrderParams",
            },
          },
        },
      ],
    },
    {
      name: "cancelOrder",
      discriminator: [95, 129, 237, 240, 8, 49, 223, 132],
      accounts: [
        {
          name: "signer",
          signer: true,
        },
        {
          name: "maker",
          writable: true,
          relations: ["order"],
        },
        {
          name: "order",
          writable: true,
        },
        {
          name: "inputMintReserve",
          writable: true,
        },
        {
          name: "makerInputMintAccount",
          writable: true,
          optional: true,
        },
        {
          name: "inputMint",
        },
        {
          name: "inputTokenProgram",
        },
        {
          name: "eventAuthority",
          pda: {
            seeds: [
              {
                kind: "const",
                value: [
                  95, 95, 101, 118, 101, 110, 116, 95, 97, 117, 116, 104, 111,
                  114, 105, 116, 121,
                ],
              },
            ],
          },
        },
        {
          name: "program",
        },
      ],
      args: [],
    },
    {
      name: "fillOrder",
      discriminator: [232, 122, 115, 25, 199, 143, 136, 162],
      accounts: [
        {
          name: "taker",
          writable: true,
          signer: true,
        },
        {
          name: "maker",
          writable: true,
          relations: ["order"],
        },
        {
          name: "order",
          writable: true,
        },
        {
          name: "takerInputMintAccount",
          writable: true,
        },
        {
          name: "takerOutputMintAccount",
          writable: true,
        },
        {
          name: "makerOutputMintAccount",
          writable: true,
          optional: true,
        },
        {
          name: "feeAccount",
          writable: true,
        },
        {
          name: "orderInputMintAccount",
          writable: true,
        },
        {
          name: "inputMint",
        },
        {
          name: "inputTokenProgram",
        },
        {
          name: "outputMint",
        },
        {
          name: "outputTokenProgram",
        },
        {
          name: "jupiterProgram",
          address: "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4",
        },
        {
          name: "systemProgram",
          address: "11111111111111111111111111111111",
        },
        {
          name: "eventAuthority",
          pda: {
            seeds: [
              {
                kind: "const",
                value: [
                  95, 95, 101, 118, 101, 110, 116, 95, 97, 117, 116, 104, 111,
                  114, 105, 116, 121,
                ],
              },
            ],
          },
        },
        {
          name: "program",
        },
      ],
      args: [
        {
          name: "params",
          type: {
            defined: {
              name: "fillOrderParams",
            },
          },
        },
      ],
    },
    {
      name: "flashFillOrder",
      discriminator: [252, 104, 18, 134, 164, 78, 18, 140],
      accounts: [
        {
          name: "taker",
          writable: true,
          signer: true,
        },
        {
          name: "maker",
          writable: true,
          relations: ["order"],
        },
        {
          name: "order",
          writable: true,
        },
        {
          name: "inputMintReserve",
          writable: true,
        },
        {
          name: "makerOutputMintAccount",
          writable: true,
          optional: true,
        },
        {
          name: "takerOutputMintAccount",
          writable: true,
          optional: true,
        },
        {
          name: "feeAccount",
          writable: true,
        },
        {
          name: "inputTokenProgram",
        },
        {
          name: "outputMint",
        },
        {
          name: "outputTokenProgram",
        },
        {
          name: "systemProgram",
          address: "11111111111111111111111111111111",
        },
        {
          name: "eventAuthority",
          pda: {
            seeds: [
              {
                kind: "const",
                value: [
                  95, 95, 101, 118, 101, 110, 116, 95, 97, 117, 116, 104, 111,
                  114, 105, 116, 121,
                ],
              },
            ],
          },
        },
        {
          name: "program",
        },
      ],
      args: [
        {
          name: "params",
          type: {
            defined: {
              name: "flashFillOrderParams",
            },
          },
        },
      ],
    },
    {
      name: "initializeOrder",
      discriminator: [133, 110, 74, 175, 112, 159, 245, 159],
      accounts: [
        {
          name: "payer",
          writable: true,
          signer: true,
        },
        {
          name: "maker",
          signer: true,
        },
        {
          name: "order",
          writable: true,
          pda: {
            seeds: [
              {
                kind: "const",
                value: [111, 114, 100, 101, 114],
              },
              {
                kind: "account",
                path: "maker",
              },
              {
                kind: "arg",
                path: "uniqueId",
              },
            ],
          },
        },
        {
          name: "inputMintReserve",
          writable: true,
          pda: {
            seeds: [
              {
                kind: "account",
                path: "order",
              },
              {
                kind: "account",
                path: "inputTokenProgram",
              },
              {
                kind: "account",
                path: "inputMint",
              },
            ],
            program: {
              kind: "const",
              value: [
                140, 151, 37, 143, 78, 36, 137, 241, 187, 61, 16, 41, 20, 142,
                13, 131, 11, 90, 19, 153, 218, 255, 16, 132, 4, 142, 123, 216,
                219, 233, 248, 89,
              ],
            },
          },
        },
        {
          name: "makerInputMintAccount",
          writable: true,
        },
        {
          name: "fee",
          pda: {
            seeds: [
              {
                kind: "const",
                value: [102, 101, 101],
              },
            ],
          },
        },
        {
          name: "referral",
          optional: true,
        },
        {
          name: "inputMint",
        },
        {
          name: "outputMint",
        },
        {
          name: "inputTokenProgram",
        },
        {
          name: "outputTokenProgram",
        },
        {
          name: "systemProgram",
          address: "11111111111111111111111111111111",
        },
        {
          name: "associatedTokenProgram",
          address: "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL",
        },
        {
          name: "eventAuthority",
          pda: {
            seeds: [
              {
                kind: "const",
                value: [
                  95, 95, 101, 118, 101, 110, 116, 95, 97, 117, 116, 104, 111,
                  114, 105, 116, 121,
                ],
              },
            ],
          },
        },
        {
          name: "program",
        },
      ],
      args: [
        {
          name: "params",
          type: {
            defined: {
              name: "initializeOrderParams",
            },
          },
        },
      ],
    },
    {
      name: "preFlashFillOrder",
      discriminator: [240, 47, 153, 68, 13, 190, 225, 42],
      accounts: [
        {
          name: "taker",
          signer: true,
        },
        {
          name: "order",
          writable: true,
        },
        {
          name: "inputMintReserve",
          writable: true,
        },
        {
          name: "takerInputMintAccount",
          writable: true,
        },
        {
          name: "inputMint",
        },
        {
          name: "inputTokenProgram",
        },
        {
          name: "instruction",
          address: "Sysvar1nstructions1111111111111111111111111",
        },
      ],
      args: [
        {
          name: "params",
          type: {
            defined: {
              name: "preFlashFillOrderParams",
            },
          },
        },
      ],
    },
    {
      name: "updateFee",
      discriminator: [232, 253, 195, 247, 148, 212, 73, 222],
      accounts: [
        {
          name: "admin",
          writable: true,
          signer: true,
        },
        {
          name: "feeAuthority",
          writable: true,
          pda: {
            seeds: [
              {
                kind: "const",
                value: [102, 101, 101],
              },
            ],
          },
        },
        {
          name: "systemProgram",
          address: "11111111111111111111111111111111",
        },
      ],
      args: [
        {
          name: "params",
          type: {
            defined: {
              name: "updateFeeParams",
            },
          },
        },
      ],
    },
    {
      name: "withdrawFee",
      discriminator: [14, 122, 231, 218, 31, 238, 223, 150],
      accounts: [
        {
          name: "admin",
          writable: true,
          signer: true,
        },
        {
          name: "feeAuthority",
          docs: ["CHECK"],
          pda: {
            seeds: [
              {
                kind: "const",
                value: [102, 101, 101],
              },
            ],
          },
        },
        {
          name: "programFeeAccount",
          writable: true,
        },
        {
          name: "adminTokenAcocunt",
          writable: true,
        },
        {
          name: "tokenProgram",
        },
        {
          name: "mint",
        },
      ],
      args: [],
    },
  ],
  accounts: [
    {
      name: "fee",
      discriminator: [24, 55, 150, 250, 168, 27, 101, 178],
    },
    {
      name: "order",
      discriminator: [134, 173, 223, 185, 77, 86, 28, 51],
    },
  ],
  events: [
    {
      name: "cancelDustOrderEvent",
      discriminator: [124, 190, 74, 28, 177, 40, 200, 220],
    },
    {
      name: "cancelOrderEvent",
      discriminator: [174, 66, 141, 17, 4, 224, 162, 77],
    },
    {
      name: "createOrderEvent",
      discriminator: [49, 142, 72, 166, 230, 29, 84, 84],
    },
    {
      name: "tradeEvent",
      discriminator: [189, 219, 127, 211, 78, 230, 97, 238],
    },
  ],
  errors: [
    {
      code: 6000,
      name: "invalidMakingAmount",
    },
    {
      code: 6001,
      name: "invalidTakingAmount",
    },
    {
      code: 6002,
      name: "invalidMaxTakingAmount",
    },
    {
      code: 6003,
      name: "invalidInputAccount",
    },
    {
      code: 6004,
      name: "invalidMakerOutputAccount",
    },
    {
      code: 6005,
      name: "invalidTakerOutputAccount",
    },
    {
      code: 6006,
      name: "invalidPair",
    },
    {
      code: 6007,
      name: "orderExpired",
    },
    {
      code: 6008,
      name: "orderNotExpired",
    },
    {
      code: 6009,
      name: "invalidAdmin",
    },
    {
      code: 6010,
      name: "invalidKeeper",
    },
    {
      code: 6011,
      name: "mathOverflow",
    },
    {
      code: 6012,
      name: "programMismatch",
    },
    {
      code: 6013,
      name: "unknownInstruction",
    },
    {
      code: 6014,
      name: "missingRepayInstructions",
    },
    {
      code: 6015,
      name: "invalidOrder",
    },
    {
      code: 6016,
      name: "invalidBorrowMakingAmount",
    },
    {
      code: 6017,
      name: "invalidExpiredDate",
    },
    {
      code: 6018,
      name: "feeBpsTooLow",
    },
    {
      code: 6019,
      name: "feeBpsTooHigh",
    },
    {
      code: 6020,
      name: "transferTaxMint",
    },
    {
      code: 6021,
      name: "invalidOutputMint",
    },
    {
      code: 6022,
      name: "invalidMinTakingAmount",
    },
    {
      code: 6023,
      name: "invalidSwapData",
    },
    {
      code: 6024,
      name: "invalidSwapInputAmount",
    },
    {
      code: 6025,
      name: "invalidSwappedAmount",
    },
    {
      code: 6026,
      name: "invalidDustOrder",
    },
    {
      code: 6027,
      name: "unsupportedMint",
    },
  ],
  types: [
    {
      name: "cancelDustOrderEvent",
      type: {
        kind: "struct",
        fields: [
          {
            name: "orderKey",
            type: "pubkey",
          },
          {
            name: "oneUsdInputMintAmount",
            type: "u64",
          },
        ],
      },
    },
    {
      name: "cancelDustOrderParams",
      type: {
        kind: "struct",
        fields: [
          {
            name: "oneUsdInputMintAmount",
            type: "u64",
          },
        ],
      },
    },
    {
      name: "cancelOrderEvent",
      type: {
        kind: "struct",
        fields: [
          {
            name: "orderKey",
            type: "pubkey",
          },
        ],
      },
    },
    {
      name: "createOrderEvent",
      type: {
        kind: "struct",
        fields: [
          {
            name: "orderKey",
            type: "pubkey",
          },
          {
            name: "maker",
            type: "pubkey",
          },
          {
            name: "inputMint",
            type: "pubkey",
          },
          {
            name: "outputMint",
            type: "pubkey",
          },
          {
            name: "inputTokenProgram",
            type: "pubkey",
          },
          {
            name: "outputTokenProgram",
            type: "pubkey",
          },
          {
            name: "makingAmount",
            type: "u64",
          },
          {
            name: "takingAmount",
            type: "u64",
          },
          {
            name: "expiredAt",
            type: {
              option: "i64",
            },
          },
          {
            name: "feeBps",
            type: "u16",
          },
          {
            name: "feeAccount",
            type: "pubkey",
          },
          {
            name: "slippageBps",
            type: "u16",
          },
        ],
      },
    },
    {
      name: "fee",
      type: {
        kind: "struct",
        fields: [
          {
            name: "normalFeeBps",
            type: "u16",
          },
          {
            name: "stableFeeBps",
            type: "u16",
          },
        ],
      },
    },
    {
      name: "fillOrderParams",
      type: {
        kind: "struct",
        fields: [
          {
            name: "inputAmount",
            type: "u64",
          },
          {
            name: "swapData",
            type: "bytes",
          },
        ],
      },
    },
    {
      name: "flashFillOrderParams",
      type: {
        kind: "struct",
        fields: [
          {
            name: "maxTakingAmount",
            type: "u64",
          },
        ],
      },
    },
    {
      name: "initializeOrderParams",
      type: {
        kind: "struct",
        fields: [
          {
            name: "uniqueId",
            type: "u64",
          },
          {
            name: "makingAmount",
            type: "u64",
          },
          {
            name: "takingAmount",
            type: "u64",
          },
          {
            name: "expiredAt",
            type: {
              option: "i64",
            },
          },
          {
            name: "feeBps",
            type: {
              option: "u16",
            },
          },
          {
            name: "slippageBps",
            type: {
              option: "u16",
            },
          },
        ],
      },
    },
    {
      name: "order",
      type: {
        kind: "struct",
        fields: [
          {
            name: "maker",
            type: "pubkey",
          },
          {
            name: "inputMint",
            type: "pubkey",
          },
          {
            name: "outputMint",
            type: "pubkey",
          },
          {
            name: "inputTokenProgram",
            type: "pubkey",
          },
          {
            name: "outputTokenProgram",
            type: "pubkey",
          },
          {
            name: "inputMintReserve",
            type: "pubkey",
          },
          {
            name: "uniqueId",
            type: "u64",
          },
          {
            name: "oriMakingAmount",
            type: "u64",
          },
          {
            name: "oriTakingAmount",
            type: "u64",
          },
          {
            name: "makingAmount",
            type: "u64",
          },
          {
            name: "takingAmount",
            type: "u64",
          },
          {
            name: "borrowMakingAmount",
            type: "u64",
          },
          {
            name: "expiredAt",
            type: {
              option: "i64",
            },
          },
          {
            name: "feeBps",
            type: "u16",
          },
          {
            name: "feeAccount",
            type: "pubkey",
          },
          {
            name: "createdAt",
            type: "i64",
          },
          {
            name: "updatedAt",
            type: "i64",
          },
          {
            name: "bump",
            type: "u8",
          },
          {
            name: "slippageBps",
            type: "u16",
          },
        ],
      },
    },
    {
      name: "preFlashFillOrderParams",
      type: {
        kind: "struct",
        fields: [
          {
            name: "makingAmount",
            type: "u64",
          },
        ],
      },
    },
    {
      name: "tradeEvent",
      type: {
        kind: "struct",
        fields: [
          {
            name: "orderKey",
            type: "pubkey",
          },
          {
            name: "taker",
            type: "pubkey",
          },
          {
            name: "remainingMakingAmount",
            type: "u64",
          },
          {
            name: "remainingTakingAmount",
            type: "u64",
          },
          {
            name: "makingAmount",
            type: "u64",
          },
          {
            name: "takingAmount",
            type: "u64",
          },
        ],
      },
    },
    {
      name: "updateFeeParams",
      type: {
        kind: "struct",
        fields: [
          {
            name: "normalFeeBps",
            type: "u16",
          },
          {
            name: "stableFeeBps",
            type: "u16",
          },
        ],
      },
    },
  ],
};
