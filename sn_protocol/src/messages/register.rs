// Copyright 2023 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use sn_registers::{Register, RegisterAddress, RegisterOp};

use serde::{Deserialize, Serialize};

/// A register cmd that is sent over to the Network
#[allow(clippy::large_enum_variant)]
#[derive(Eq, PartialEq, Clone, Serialize, Deserialize, Debug)]
pub enum RegisterCmd {
    /// Create a new register on the network.
    Create {
        /// The base register (contains, owner, name, tag, permissions, and register initial state)
        register: Register,
        /// The signature of the owner on that register.
        signature: bls::Signature,
    },
    /// Edit the register
    Edit(RegisterOp),
}

impl RegisterCmd {
    /// Returns the dst address of the register.
    pub fn dst(&self) -> RegisterAddress {
        match self {
            Self::Create { register, .. } => *register.address(),
            Self::Edit(op) => op.address(),
        }
    }
}
